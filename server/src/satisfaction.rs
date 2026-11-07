//! 满意度刷新。
//!
//! 口径（社区接口）：
//! 1. `POST /japi/v1/judgement/listJudgedReport` 的列表项 `id` 就是案件号，
//!    `judgeRecordId` 才是取满意度要用的「判决记录号」；
//! 2. `POST /japi/v1/judgement/myOpinion` 返回 `userAgreeNum / userDisagreeNum`，
//!    其中赞同数含审核员自己那一票，减 1 后自己算满意度（上游的 ratio 不准）；
//! 3. 记录只要还在库里（没被删除），每 24 小时刷新一次，避免数据更新不及时。

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use serde_json::{json, Value};

use crate::{
    auto::AutoRuntime,
    community::{satisfaction_from_counts, Community},
    log,
    records::{self, RecordsStore},
};

/// 后台检查间隔：每 10 分钟看一次有没有到期的记录（真正刷新仍按 24 小时）。
const TICK: Duration = Duration::from_secs(10 * 60);
/// 单轮最多刷新多少条，避免把社区接口刷爆。
const PER_TICK: i64 = 12;
/// 客户端按需刷新时最多扫多少页判决列表（每页 50 条）。
pub const DISCOVER_PAGES: u32 = 20;
/// 后台每日刷新时多扫几页，尽量把老案件也找回来。
const BACKGROUND_PAGES: u32 = 40;

/// 一条记录的刷新结果。
pub struct Refreshed {
    pub case_id: i64,
    pub judge_record_id: i64,
    /// 0~1；None 表示还没有其他用户评价（或上游没给数据）。
    pub satisfaction: Option<f32>,
    /// 这次是否真正取到了满意度；false 表示上游报错，下次再试。
    pub fetched: bool,
    /// 这条记录这种来源的获取机会已经用过了（手动/自动各一次）。
    pub skipped: bool,
}

impl Refreshed {
    pub fn to_json(&self) -> Value {
        json!({
            "caseId": self.case_id,
            "judgeRecordId": self.judge_record_id,
            "satisfaction": self.satisfaction,
            "fetched": self.fetched,
            "skipped": self.skipped,
        })
    }
}

/// 批量刷新满意度。
///
/// 判决列表只扫一遍：批量按案件号匹配 `judgeRecordId`，再逐个请求 myOpinion。
/// 匹配不到时退回调用方给的 `judgeRecordId`（老数据兜底）。
pub async fn refresh_batch(
    records: &RecordsStore,
    community: &Community,
    token: &str,
    account_key: Option<&str>,
    items: &[(i64, i64)],
    max_pages: u32,
    source: &str,
) -> Vec<Refreshed> {
    let wanted: Vec<i64> = items.iter().map(|(case_id, _)| *case_id).collect();
    // 「正常显示」的案件没有满意度：按记录里的 action 直接跳过，
    // 避免客户端传错或旧客户端继续抓取。
    let excluded: HashSet<i64> = match account_key {
        Some(key) => records
            .normal_display_cases(key, &wanted)
            .await
            .unwrap_or_default(),
        None => HashSet::new(),
    };
    let index = match community.judged_index(token, &wanted, max_pages).await {
        Ok(index) => index,
        Err(error) => {
            log(format!("[satisfaction] 读取判决列表失败：{error}"));
            HashMap::new()
        }
    };

    let mut results = Vec::with_capacity(items.len());
    // 每个案件手动 / 自动各只有一次机会，只有真正取到值才算用掉。
    let slots = match account_key {
        Some(key) => records.satisfaction_slots(key, &wanted).await.unwrap_or_default(),
        None => HashMap::new(),
    };
    for (case_id, hint) in items {
        if *case_id <= 0 || excluded.contains(case_id) {
            continue;
        }
        let (manual_used, auto_used) = slots.get(case_id).copied().unwrap_or((false, false));
        let already_used = if source == "manual" { manual_used } else { auto_used };
        if already_used {
            results.push(Refreshed {
                case_id: *case_id,
                judge_record_id: 0,
                satisfaction: None,
                fetched: false,
                skipped: true,
            });
            continue;
        }
        let entry = index.get(case_id);
        let judge_record_id = entry
            .map(|item| item.judge_record_id)
            .filter(|id| *id > 0)
            .unwrap_or((*hint).max(0));
        if judge_record_id <= 0 {
            // 判决列表里还没有这个案件（没出判决 / 还没被处罚），下一轮再试，不消耗机会。
            log(format!("[satisfaction] 案件#{case_id} 还不在判决列表里，稍后再试"));
            if let Some(key) = account_key {
                let _ = records.mark_satisfaction_attempt(key, *case_id).await;
            }
            results.push(Refreshed {
                case_id: *case_id,
                judge_record_id: 0,
                satisfaction: None,
                fetched: false,
                skipped: false,
            });
            continue;
        }

        let mut result = Refreshed {
            case_id: *case_id,
            judge_record_id,
            satisfaction: None,
            fetched: false,
            skipped: false,
        };
        // 判决列表自带 opinionInfo 时直接算：已经投过票的案件再调 myOpinion 会回 107005。
        let outcome = match entry.and_then(|item| item.opinion) {
            Some((agree, disagree)) => Ok(satisfaction_from_counts(agree, disagree)),
            None => community.satisfaction(token, judge_record_id).await,
        };
        match outcome {
            Ok(Some(satisfaction)) => {
                result.satisfaction = Some(satisfaction);
                result.fetched = true;
                if let Some(key) = account_key {
                    if let Err(error) = records
                        .set_satisfaction_by_key(key, *case_id, judge_record_id, Some(satisfaction), source)
                        .await
                    {
                        log(format!("[satisfaction] 案件#{case_id} 写回失败：{error}"));
                    }
                }
            }
            Ok(None) => {
                // 还没有其他用户投票：不算取到，机会保留，只记一次时间过一阵再试。
                if let Some(key) = account_key {
                    let _ = records.mark_satisfaction_attempt(key, *case_id).await;
                }
            }
            Err(error) => {
                log(format!("[satisfaction] 案件#{case_id} 取满意度失败：{error}"));
                // 失败不消耗机会，只记一次时间，后台下一轮不会马上再撞同一个案件。
                if let Some(key) = account_key {
                    let _ = records.mark_satisfaction_attempt(key, *case_id).await;
                }
            }
        }
        results.push(result);
    }
    results
}

/// 后台任务：本地记录还在就一直每日刷新一次满意度，不依赖浏览器是否打开。
pub fn spawn(records: RecordsStore, community: Community, auto: AutoRuntime) {
    tokio::spawn(async move {
        // 启动时先让自动审核循环跑起来，避免两件事挤在同一时刻抢社区接口。
        tokio::time::sleep(Duration::from_secs(90)).await;
        loop {
            if let Err(error) = run_once(&records, &community, &auto).await {
                log(format!("[satisfaction] 本轮刷新失败：{error}"));
            }
            tokio::time::sleep(TICK).await;
        }
    });
}

async fn run_once(
    records: &RecordsStore,
    community: &Community,
    auto: &AutoRuntime,
) -> Result<(), String> {
    // 开源版没有账号库，后台只有 auto.json / AUTO_TOKEN 里那把 token 可用；
    // 页面上的按需刷新（POST /api/records/satisfaction/refresh）走请求方自己的 token。
    let token = auto.current_token();
    if token.trim().is_empty() {
        return Ok(());
    }
    let key = records::token_hash(&token);
    let due: Vec<(i64, i64)> = records
        .due_satisfaction(PER_TICK)
        .await?
        .into_iter()
        .filter(|job| job.account_key == key)
        .map(|job| (job.case_id, job.judge_record_id))
        .collect();
    if due.is_empty() {
        return Ok(());
    }
    // 判决列表只扫一遍：批量按案件号匹配 judgeRecordId。
    let results = refresh_batch(records, community, &token, Some(&key), &due, BACKGROUND_PAGES, "auto").await;
    let fetched = results.iter().filter(|item| item.fetched).count();
    log(format!(
        "[satisfaction] 本轮刷新 {fetched}/{} 条满意度",
        due.len()
    ));
    Ok(())
}

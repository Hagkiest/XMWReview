//! 服务器端 7×24 自动审核：不依赖浏览器开着页面，由 tokio 定时任务独立执行。
//!
//! 配置持久化在 exe 同目录的 auto.json 里（token、开关、间隔、阈值），
//! 重启后自动恢复；运行统计只在内存中按天累计。

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    ai::{self, AiConfig, USER_DELETE_MESSAGE},
    community::{CaseDetail, Community},
    log,
};

/// 超过这么久没人处理的案件不再自动审理（与前端口径一致：7 天）。
const CASE_MAX_AGE_SECS: i64 = 7 * 24 * 60 * 60;
/// 同一案件失败后的最大尝试次数；用尽后当天不再碰，避免反复撞墙。
const MAX_ATTEMPTS: u32 = 5;
const PER_TICK: usize = 3;
/// 单轮总时限（秒）：超时就结束本轮，等下一轮继续。
const TICK_BUDGET_SECS: i64 = 180;
/// 轮次序号，仅用于日志，便于确认"到底有没有在跑"。
static TICK_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase", default)]
pub struct AutoConfig {
    pub enabled: bool,
    /// 用哪个审核员的身份提交；留空表示尚未配置。
    pub token: String,
    /// 拉取间隔（秒）。
    pub interval: u64,
    /// 低于该置信度的判定转人工（只记录，不提交）。
    pub threshold: f64,
    /// AI 判为「替换」时按「删除」提交，绕开上游对 action=2 的参数错误。
    pub prefer_delete: bool,
}

impl Default for AutoConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            token: String::new(),
            interval: 60,
            threshold: 0.6,
            prefer_delete: true,
        }
    }
}

impl AutoConfig {
    /// 配置文件路径：默认系统盘数据目录（更新/重装 exe 不会丢）；可用 AUTO_JSON 覆盖。
    fn config_path(_exe_dir: &PathBuf) -> PathBuf {
        match std::env::var("AUTO_JSON") {
            Ok(value) if !value.trim().is_empty() => PathBuf::from(value.trim()),
            _ => crate::data_dir().join("auto.json"),
        }
    }

    /// 首次启动（没有 auto.json）时用环境变量做初始值。
    fn from_env() -> Self {
        let mut config = Self::default();
        if let Ok(value) = std::env::var("AUTO_TOKEN") {
            config.token = value.trim().to_string();
        }
        if let Ok(value) = std::env::var("AUTO_ENABLED") {
            config.enabled = value.trim() == "1" || value.eq_ignore_ascii_case("true");
        }
        if let Ok(value) = std::env::var("AUTO_INTERVAL") {
            if let Ok(seconds) = value.trim().parse::<u64>() {
                config.interval = seconds.clamp(10, 3600);
            }
        }
        if let Ok(value) = std::env::var("AUTO_THRESHOLD") {
            if let Ok(value) = value.trim().parse::<f64>() {
                config.threshold = value.clamp(0.0, 1.0);
            }
        }
        if let Ok(value) = std::env::var("AUTO_PREFER_DELETE") {
            config.prefer_delete = value.trim() != "0" && !value.eq_ignore_ascii_case("false");
        }
        config
    }

    fn load(exe_dir: &PathBuf) -> Self {
        let path = Self::config_path(exe_dir);
        // 旧版本配置写在 exe 同目录，首次启动复制到系统盘数据目录（不删旧文件）。
        if !path.exists() && std::env::var("AUTO_JSON").is_err() {
            let legacy = exe_dir.join("auto.json");
            if legacy.exists() {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::copy(&legacy, &path);
            }
        }
        if let Ok(raw) = std::fs::read_to_string(&path) {
            match serde_json::from_str::<AutoConfig>(&raw) {
                Ok(mut config) => {
                    config.interval = config.interval.clamp(10, 3600);
                    config.threshold = config.threshold.clamp(0.0, 1.0);
                    config.token = config.token.trim().to_string();
                    return config;
                }
                Err(error) => log(format!("auto.json 解析失败，改用默认配置：{error}")),
            }
        }
        Self::from_env()
    }

    fn save(&self, exe_dir: &PathBuf) {
        let path = Self::config_path(exe_dir);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(raw) = serde_json::to_string_pretty(self) {
            if let Err(error) = std::fs::write(&path, raw) {
                log(format!("auto.json 写入失败：{error}"));
            }
        }
    }
}

/// 今日统计（服务器本地时区近似按 UTC 天切分，仅用于值班观察）。
#[derive(Default, Clone)]
struct DayStats {
    day: i64,
    submitted: u32,
    waiting: u32,
    failed: u32,
    stale: u32,
}

#[derive(Clone)]
struct Cooldown {
    until: SystemTime,
    attempts: u32,
}

/// 由 HTTP 处理器与后台任务共享的运行时状态。
#[derive(Clone)]
pub struct AutoRuntime {
    config: Arc<RwLock<AutoConfig>>,
    stats: Arc<Mutex<DayStats>>,
    handled: Arc<Mutex<HashSet<i64>>>,
    cooldowns: Arc<Mutex<HashMap<i64, Cooldown>>>,
    /// 退避表对应的自然日，跨天时清空。
    cooldown_day: Arc<Mutex<i64>>,
    exe_dir: PathBuf,
    last_run: Arc<Mutex<Option<i64>>>,
    last_message: Arc<Mutex<String>>,
}

impl AutoRuntime {
    pub fn new(exe_dir: &PathBuf) -> Self {
        let config = AutoConfig::load(exe_dir);
        if config.enabled && config.token.is_empty() {
            log("服务器自动审核已启用但没有配置 token，暂不运行");
        }
        Self {
            config: Arc::new(RwLock::new(config)),
            stats: Arc::new(Mutex::new(DayStats::default())),
            handled: Arc::new(Mutex::new(HashSet::new())),
            cooldowns: Arc::new(Mutex::new(HashMap::new())),
            cooldown_day: Arc::new(Mutex::new(unix_day())),
            exe_dir: exe_dir.clone(),
            last_run: Arc::new(Mutex::new(None)),
            last_message: Arc::new(Mutex::new(String::new())),
        }
    }

    /// 当前正在用的审核 token：满意度后台刷新复用它，
    /// 避免托管审核记录的 token 只存在 auto.json 里时取不到。
    pub fn current_token(&self) -> String {
        self.read_config().token.trim().to_string()
    }

    fn read_config(&self) -> AutoConfig {
        self.config
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// 应用一次配置变更并持久化到 auto.json。
    pub fn update<F: FnOnce(&mut AutoConfig)>(&self, patch: F) -> AutoConfig {
        let mut config = self.read_config();
        patch(&mut config);
        config.interval = config.interval.clamp(10, 3600);
        config.threshold = config.threshold.clamp(0.0, 1.0);
        config.token = config.token.trim().to_string();
        config.save(&self.exe_dir);
        *self
            .config
            .write()
            .unwrap_or_else(|error| error.into_inner()) = config.clone();
        config
    }

    fn bump(&self, field: fn(&mut DayStats) -> &mut u32) {
        let mut stats = self
            .stats
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let today = unix_day();
        if stats.day != today {
            *stats = DayStats { day: today, ..Default::default() };
        }
        *field(&mut stats) += 1;
    }

    fn set_message(&self, message: String) {
        *self
            .last_message
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = message;
    }

    /// 跨天清空失败退避：否则"重试次数用尽"的案件会永久跳过。
    fn reset_cooldowns_daily(&self) {
        let today = unix_day();
        let mut day = self
            .cooldown_day
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *day == today {
            return;
        }
        let previous = *day;
        *day = today;
        self.cooldowns
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        if previous != 0 {
            log(format!("[server-auto] 进入新的自然日（{today}），失败退避已重置"));
        }
    }

    /// GET /api/auto-config 的响应体（token 永远只回掩码）。
    pub fn status(&self) -> serde_json::Value {
        let config = self.read_config();
        let stats = self
            .stats
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let today = unix_day();
        let stats = if stats.day == today {
            stats
        } else {
            DayStats { day: today, ..Default::default() }
        };
        let last_run = *self
            .last_run
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        json!({
            "enabled": config.enabled,
            "configured": !config.token.is_empty(),
            "tokenMasked": mask_token(&config.token),
            "interval": config.interval,
            "threshold": config.threshold,
            "preferDelete": config.prefer_delete,
            "lastRunAt": last_run,
            "lastMessage": self
                .last_message
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
            "today": {
                "submitted": stats.submitted,
                "waiting": stats.waiting,
                "failed": stats.failed,
                "stale": stats.stale,
            },
        })
    }
}

/// 后台循环：每 interval 秒扫一轮未认领案件并自动处理。
pub fn spawn(
    runtime: AutoRuntime,
    community: Community,
    http: reqwest::Client,
    ai: Option<AiConfig>,
    claims: crate::case_times::ClaimCache,
) {
    tokio::spawn(async move {
        // 启动后先等一会儿，让 HTTP 服务先就绪。
        tokio::time::sleep(Duration::from_secs(5)).await;
        loop {
            let config = runtime.read_config();
            let token = config.token.trim().to_string();
            if config.enabled && !token.is_empty() {
                // 每轮放进独立 task：单轮 panic 只影响这一轮，不会把循环整个搞死
                //（之前一次异常就会让 24 小时审核静默停摆）。
                let tick_runtime = runtime.clone();
                let tick_community = community.clone();
                let tick_http = http.clone();
                let tick_config = config.clone();
                let tick_ai = ai.clone();
                let tick_claims = claims.clone();
                let join = tokio::spawn(async move {
                    run_tick(
                        &tick_runtime,
                        &tick_community,
                        &tick_http,
                        &tick_config,
                        &token,
                        tick_ai.as_ref(),
                        &tick_claims,
                    )
                    .await;
                });
                if let Err(error) = join.await {
                    let reason = if error.is_panic() { "本轮发生 panic" } else { "本轮被取消" };
                    log(format!("[server-auto] {reason}，已跳过并继续下一轮：{error}"));
                    runtime.set_message(format!("上一轮异常（{reason}），已自动恢复"));
                }
                tokio::time::sleep(Duration::from_secs(config.interval.clamp(10, 3600))).await;
            } else {
                // 未启用时低频空转，配置热更新后最多 15 秒生效。
                runtime.set_message(if token.is_empty() {
                    "尚未配置审核 token（在设置里填写，或写进 auto.json / AUTO_TOKEN）".to_string()
                } else {
                    "自动审核已暂停".to_string()
                });
                tokio::time::sleep(Duration::from_secs(15)).await;
            }
        }
    });
}

/// 一轮：扫 3 页 → 逐件分析提交。与前端 processOne 的规则保持一致。
async fn run_tick(
    runtime: &AutoRuntime,
    community: &Community,
    http: &reqwest::Client,
    config: &AutoConfig,
    token: &str,
    ai: Option<&AiConfig>,
    claims: &crate::case_times::ClaimCache,
) {
    let Some(ai) = ai else {
        runtime.set_message("后端未配置 AI 服务，无法自动判定".to_string());
        return;
    };
    let tick = TICK_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    // 新的自然日重置失败退避，避免"永久跳过"越积越多。
    runtime.reset_cooldowns_daily();
    let started = now_secs();
    // 单轮总时限：某个请求卡住也不会把这一轮拖死。
    let deadline = started + TICK_BUDGET_SECS;
    let mut submitted = 0usize;
    let mut waiting = 0usize;
    let mut failed = 0usize;
    let mut stale = 0usize;
    let mut candidates = 0usize;

    let mut cases: Vec<_> = Vec::new();
    for page in 1..=3u32 {
        match community.cases(token, page, 50, None).await {
            Ok(result) => {
                let total = result.list.len();
                cases.extend(result.list);
                if total < 50 {
                    break;
                }
            }
            Err(error) => {
                runtime.set_message(format!("拉取案件列表失败：{error}"));
                log(format!("[server-auto] 拉取案件列表失败：{error}"));
                runtime.bump(|stats| &mut stats.failed);
                return;
            }
        }
    }

    // 列表接口不返回认领信息：多次举报的案件必须看详情里最新那条举报的 checkerName，
    // 否则会把这个还没人认领的案件当成「已认领」跳过。
    let mut resolved = 0usize;
    for item in cases.iter_mut() {
        if item.report_num <= 1 {
            continue;
        }
        if let Some(cached) = claims.get(item.id) {
            item.checker_name = cached;
            continue;
        }
        if resolved >= 5 {
            break;
        }
        resolved += 1;
        if let Ok(Ok(detail)) = tokio::time::timeout(
            Duration::from_secs(4),
            community.case_detail(token, item.id),
        )
        .await
        {
            if let Some(name) = detail.latest_checker_name() {
                claims.set(item.id, &name);
                item.checker_name = name;
            }
        }
    }

    let now = now_secs();
    let handled = runtime
        .handled
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    for item in &cases {
        let claimable = first_unclaimed(&item.checker_name)
            && !handled.contains(&item.id)
            && cooldown_until(runtime, item.id) <= now;
        if claimable {
            candidates += 1;
        }
    }

    for item in cases {
        if submitted + waiting + failed >= PER_TICK {
            break;
        }
        // 单轮超时：剩下的留到下一轮，避免整轮卡死。
        if now_secs() > deadline {
            log(format!("[server-auto] 第 {tick} 轮超过 {TICK_BUDGET_SECS}s，剩余案件留到下一轮"));
            break;
        }
        if !first_unclaimed(&item.checker_name)
            || handled.contains(&item.id)
            || cooldown_until(runtime, item.id) > now_secs()
        {
            continue;
        }

        // 详情确认仍无人认领，同时拿到准确举报时间用于超期判断。
        let detail: CaseDetail = match community.case_detail(token, item.id).await {
            Ok(detail) => detail,
            Err(error) => {
                mark_failure(runtime, item.id, &format!("拉取详情失败：{error}"));
                failed += 1;
                runtime.bump(|stats| &mut stats.failed);
                continue;
            }
        };
        if detail.checker_id != 0 {
            runtime
                .handled
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(item.id);
            continue;
        }
        if is_stale(&detail) {
            runtime
                .handled
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(item.id);
            stale += 1;
            runtime.bump(|stats| &mut stats.stale);
            log(format!("[server-auto] 案件#{} 超过一周未审理，跳过", item.id));
            continue;
        }

        let suggestion = match ai::suggest(http, ai, &detail).await {
            Ok(suggestion) => suggestion,
            Err(error) => {
                mark_failure(runtime, item.id, &format!("AI 判定失败：{error}"));
                failed += 1;
                runtime.bump(|stats| &mut stats.failed);
                log(format!("[server-auto] 案件#{} AI 判定失败：{error}", item.id));
                continue;
            }
        };

        // 优先删除：与前端一致，绕开上游对 action=2 的参数错误。
        let action = if config.prefer_delete && suggestion.action == 2 {
            3u8
        } else {
            suggestion.action as u8
        };

        if suggestion.confidence < config.threshold {
            // 低置信度转人工：记录后不再自动重试，等审核员在界面里处理。
            runtime
                .handled
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(item.id);
            waiting += 1;
            runtime.bump(|stats| &mut stats.waiting);
            log(format!(
                "[server-auto] 案件#{} 置信度 {:.0}% 低于阈值，转人工（建议 action={}）",
                item.id,
                suggestion.confidence * 100.0,
                suggestion.action
            ));
            continue;
        }

        let user_message = if action == 3 && config.prefer_delete && suggestion.action == 2 {
            USER_DELETE_MESSAGE.to_string()
        } else {
            suggestion.user_message.clone()
        };
        match submit(community, token, item.id, action, &suggestion.remark, &user_message, &suggestion.reporter_message).await {
            Ok((case, reports)) => {
                submitted += 1;
                runtime.bump(|stats| &mut stats.submitted);
                runtime
                    .handled
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .insert(item.id);
                runtime
                    .cooldowns
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .remove(&item.id);
                log(format!(
                    "[server-auto] 提交成功 案件#{}: action={action} 举报记录={reports} 条 remark={}",
                    case.report_id, suggestion.remark
                ));
                // 云端不再保存历史记录，后续改为遍历已审核案件列表并按 checkerName 同步。
            }
            Err(error) => {
                mark_failure(runtime, item.id, &error);
                failed += 1;
                runtime.bump(|stats| &mut stats.failed);
                log(format!("[server-auto] 案件#{} 提交失败：{error}", item.id));
            }
        }
    }

    *runtime
        .last_run
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(started * 1000);
    // 每轮都留一行日志：能直接看出自动审核有没有在跑、跑了多少。
    log(format!(
        "[server-auto] 第 {tick} 轮完成（{}s）：候选 {candidates}，提交 {submitted}，转人工 {waiting}，失败 {failed}，超期 {stale}",
        now_secs() - started
    ));
    let message = if submitted + waiting + failed == 0 {
        if candidates == 0 {
            "本轮没有未认领的可处理案件".to_string()
        } else {
            format!("{candidates} 件未认领案件在退避或已处理过，等下一轮")
        }
    } else {
        let mut parts = Vec::new();
        if submitted > 0 {
            parts.push(format!("自动处理 {submitted} 件"));
        }
        if waiting > 0 {
            parts.push(format!("转人工 {waiting} 件"));
        }
        if failed > 0 {
            parts.push(format!("失败 {failed} 件"));
        }
        if stale > 0 {
            parts.push(format!("{stale} 件超期跳过"));
        }
        parts.join(" · ")
    };
    runtime.set_message(message);
}

async fn submit(
    community: &Community,
    token: &str,
    id: i64,
    action: u8,
    remark: &str,
    user_message: &str,
    reporter_message: &str,
) -> Result<(CaseDetail, usize), String> {
    match community
        .verify_case(token, id, action, remark, user_message, reporter_message)
        .await
    {
        Ok(result) => Ok(result),
        Err(error) => {
            // action=2 偶发上游参数错误，改删除重试一次（与前端兜底一致）。
            if action == 2 {
                log(format!("[server-auto] 案件#{id} 替换失败，改用删除重试：{error}"));
                return community
                    .verify_case(token, id, 3, remark, USER_DELETE_MESSAGE, reporter_message)
                    .await
                    .map_err(|retry| format!("{error}；删除重试也失败：{retry}"));
            }
            Err(error.to_string())
        }
    }
}

/// 失败退避：1、2、4、8、16 分钟；达到上限后当天交给人工。
fn mark_failure(runtime: &AutoRuntime, id: i64, message: &str) {
    let mut cooldowns = runtime
        .cooldowns
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let attempts = cooldowns.get(&id).map(|c| c.attempts).unwrap_or(0) + 1;
    let backoff = Duration::from_secs(60u64.saturating_mul(1 << (attempts - 1).min(4)));
    cooldowns.insert(
        id,
        Cooldown {
            until: SystemTime::now() + backoff,
            attempts,
        },
    );
    log(format!(
        "[server-auto] 案件#{id} 第 {attempts}/{MAX_ATTEMPTS} 次失败：{message}"
    ));
}

fn cooldown_until(runtime: &AutoRuntime, id: i64) -> i64 {
    let cooldowns = runtime
        .cooldowns
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    cooldowns
        .get(&id)
        .map(|c| {
            if c.attempts >= MAX_ATTEMPTS {
                i64::MAX
            } else {
                c.until
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0)
            }
        })
        .unwrap_or(0)
}

/// 社区接口用空串或横线（-、--、—）表示未认领。
pub(crate) fn unclaimed(checker_name: &str) -> bool {
    let name = checker_name.trim();
    name.is_empty()
        || name
            .chars()
            .all(|c| matches!(c, '-' | '–' | '—' | ' ' | '\u{3000}'))
}

/// 同一案件可能有多条举报记录，checkerName 也可能是「甲,乙」这种拼接串。
/// 认领情况只看**第一个出现**的审核员名：它是空或横线，才算未认领。
pub(crate) fn first_unclaimed(checker_name: &str) -> bool {
    let first = checker_name
        .split(|c: char| matches!(c, ',' | '，' | ';' | '；' | '/' | '|' | '\n' | '\t'))
        .next()
        .unwrap_or("");
    unclaimed(first)
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn unix_day() -> i64 {
    now_secs() / 86_400
}

/// 解析社区接口的时间字符串（"2026-01-14 12:20:31"、"26-1-14 12:20"）。
pub(crate) fn parse_when(text: &str) -> Option<i64> {
    let trimmed = text.trim();
    let (date, time) = match trimmed.split_once(' ') {
        Some((date, time)) => (date, Some(time)),
        None => (trimmed, None),
    };
    let mut parts = date.split(|c: char| c == '-' || c == '/' || c == '年' || c == '月' || c == '.');
    let raw_year: i64 = parts.next()?.trim().parse().ok()?;
    let month: i64 = parts.next()?.trim().parse().ok()?;
    let day: i64 = parts.next()?.trim().parse().ok()?;
    let year = if (0..100).contains(&raw_year) { 2000 + raw_year } else { raw_year };
    let (hour, minute) = match time {
        Some(time) => {
            let mut parts = time.split(':');
            (
                parts.next()?.trim().parse().ok().unwrap_or(0),
                parts.next().and_then(|v| v.trim().parse().ok()).unwrap_or(0),
            )
        }
        None => (0, 0),
    };
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60)
}

/// Howard Hinnant 的 civil-from-days 算法（按 UTC 近似，用于超期判断足够）。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 列表里的举报时间超过一周 → 视为超期。
/// 时间缺失/无法解析时返回 false，调用方需要单独按“时间未知”处理（不能再当可审理）。
pub(crate) fn is_stale_text(created_at: &str) -> bool {
    match parse_when(created_at) {
        Some(when) => now_secs() - when > CASE_MAX_AGE_SECS,
        None => false,
    }
}

/// Unix 秒版本的超期判断（作品列表只有 lastReportTime）。
pub(crate) fn is_stale_unix(timestamp: i64) -> bool {
    timestamp > 0 && now_secs() - timestamp > CASE_MAX_AGE_SECS
}

/// 判断案件是否超过一周没人处理。
/// 口径是「最早一条举报的举报时间」；时间解析不出来时按超期处理，
/// 因为这类案件通常是被举报评论的旧数据，不能再自动分析/提交。
pub(crate) fn is_stale(detail: &CaseDetail) -> bool {
    let earliest = detail
        .reports
        .iter()
        .filter_map(|report| parse_when(&report.created_at))
        .min();
    match earliest {
        Some(earliest) => now_secs() - earliest > CASE_MAX_AGE_SECS,
        None => true,
    }
}

fn mask_token(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() <= 8 {
        return "****".to_string();
    }
    let head: String = chars.iter().take(4).collect();
    let tail: String = chars.iter().skip(chars.len() - 4).collect();
    format!("{head}…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_time_formats() {
        let base = days_from_civil(2026, 1, 14) * 86_400;
        assert_eq!(parse_when("2026-01-14 12:20:31"), Some(base + 12 * 3600 + 20 * 60));
        assert_eq!(parse_when("26-1-14 12:20"), Some(base + 12 * 3600 + 20 * 60));
        assert_eq!(parse_when("2026/1/14"), Some(base));
        assert_eq!(parse_when("垃圾"), None);
    }

    #[test]
    fn checker_dash_means_unclaimed() {
        assert!(unclaimed(""));
        assert!(unclaimed("-"));
        assert!(unclaimed("--"));
        assert!(unclaimed("—"));
        assert!(unclaimed(" – "));
        assert!(!unclaimed("张三"));
    }

    #[test]
    fn first_checker_decides_claim() {
        // 多次举报会把多个审核员名拼在一起，只看第一个出现的那个。
        assert!(first_unclaimed("--"));
        assert!(first_unclaimed("--,张三"));
        assert!(first_unclaimed(" , 张三"));
        assert!(!first_unclaimed("张三,--"));
        assert!(!first_unclaimed("张三"));
    }

    #[test]
    fn token_mask_never_reveals_secrets() {
        assert_eq!(mask_token("short"), "****");
        let masked = mask_token("sk-notarealkeyABCDEF0123456789abcdef");
        assert_eq!(masked, "sk-n…cdef");
        assert!(!masked.contains("arealkey"));
    }
}

use reqwest::{Client, Method};
use serde::Serialize;
use serde_json::{json, Value};

use crate::error::ApiError;

pub const TOKEN_HEADER: &str = "access-token";

/// 判决列表每页条数。
const JUDGED_PAGE_SIZE: u32 = 50;

/// Reads integers that the community API sometimes sends as strings.
pub fn int(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
}

pub fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => String::new(),
    }
}

/// 「我已判决」列表中的一条：列表项的 id 就是案件号。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JudgedRef {
    pub case_id: i64,
    pub judge_record_id: i64,
    /// 判决列表自带 `opinionInfo` 的赞同/反对计数（含审核员自己那一票）。
    /// 有它就不必再请求 myOpinion——那个接口对已经投过票的案件会回 107005。
    pub opinion: Option<(i64, i64)>,
}

/// 满意度 = (赞同数 - 1) / ((赞同数 - 1) + 反对数)。
///
/// 赞同数含审核员自己那一票，减 1 才是其他用户的赞同数；
/// 上游返回的 userAgreeRatio 口径不准（和实际计数对不上），所以只拿计数自己算。
/// 除审核员自己外还没人评价时返回 None。
pub fn satisfaction_from_counts(agree: i64, disagree: i64) -> Option<f32> {
    let agree = (agree - 1).max(0);
    let disagree = disagree.max(0);
    let total = agree + disagree;
    if total <= 0 {
        return None;
    }
    Some((agree as f32 / total as f32).clamp(0.0, 1.0))
}

pub fn opinion_satisfaction(value: &Value) -> Option<f32> {
    satisfaction_from_counts(
        int(&value["userAgreeNum"]).unwrap_or(0),
        int(&value["userDisagreeNum"]).unwrap_or(0),
    )
}

#[derive(Clone)]
pub struct Community {
    http: Client,
    base_url: String,
}

impl Community {
    pub fn new(http: Client, base_url: String) -> Self {
        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    async fn call(
        &self,
        token: &str,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Value, ApiError> {
        self.call_base(&self.base_url, token, method, path, query, body)
            .await
    }

    /// 判决相关接口在 /japi/v1 下，其余在 /api/v1 下，所以基址作为参数传入。
    async fn call_base(
        &self,
        base: &str,
        token: &str,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Value, ApiError> {
        let mut request = self
            .http
            .request(method, format!("{base}{path}"))
            .header(TOKEN_HEADER, token)
            .query(query);
        if let Some(body) = body {
            request = request.json(body);
        }

        let response = request
            .send()
            .await
            .map_err(|error| ApiError::Upstream(format!("无法连接社区接口：{error}")))?;
        let payload: Value = response
            .json()
            .await
            .map_err(|error| ApiError::Upstream(format!("社区接口响应无法解析：{error}")))?;

        let code = payload.get("code").and_then(int);
        // 两套接口成功码不同：/api/v1 用 0，/japi/v1 用 200（message 为 "ok"）。
        // 旧实现只认 0，导致判决记录与满意度接口一直被当成失败，满意度永远取不到。
        let success = matches!(code, Some(0) | Some(200));
        if !success {
            let query = query.iter().map(|(key, value)| format!("{key}={value}")).collect::<Vec<_>>().join("&");
            let mut message = format!(
                "社区接口异常 {}?{} code={:?} message={}",
                path,
                query,
                code,
                text(&payload["message"])
            );
            if let Some(body) = body {
                message.push_str(&format!("\n请求体：{body}"));
            }
            crate::log(message);
        }

        match code {
            Some(10021) => Err(ApiError::SessionExpired),
            Some(code) if success => {
                let _ = code;
                Ok(payload.get("data").cloned().unwrap_or(Value::Null))
            }
            Some(code) => {
                let message = text(&payload["message"]);
                Err(ApiError::Business {
                    code,
                    message: if message.is_empty() { "社区接口返回异常".to_string() } else { message },
                })
            }
            None => Err(ApiError::Upstream("社区接口响应缺少状态码".to_string())),
        }
    }

    /// /api/v1 → /japi/v1；也允许用 COMMUNITY_JAPI_BASE 单独指定。
    fn japi_base(&self) -> String {
        match std::env::var("COMMUNITY_JAPI_BASE") {
            Ok(value) if !value.trim().is_empty() => value.trim().trim_end_matches('/').to_string(),
            _ => self.base_url.replace("/api/v1", "/japi/v1"),
        }
    }

    /// 我审核过的案件列表（判决记录），用于历史记录与满意度。
    pub async fn judged_reports(&self, token: &str, page: u32, page_size: u32) -> Result<Value, ApiError> {
        let body = json!({ "reportType": 3, "page": page, "pageSize": page_size });
        self.call_base(&self.japi_base(), token, Method::POST, "/judgement/listJudgedReport", &[], Some(&body))
            .await
    }

    /// 某条判决记录收到的赞同 / 反对数（agreeType = 0）。
    pub async fn my_opinion(&self, token: &str, judge_record_id: i64) -> Result<Value, ApiError> {
        let body = json!({ "agreeType": 0, "judgeRecordId": judge_record_id });
        self.call_base(&self.japi_base(), token, Method::POST, "/judgement/myOpinion", &[], Some(&body))
            .await
    }

    /// 某条判决记录的满意度；没有其他用户评价时返回 None。
    pub async fn satisfaction(&self, token: &str, judge_record_id: i64) -> Result<Option<f32>, ApiError> {
        if judge_record_id <= 0 {
            return Ok(None);
        }
        let data = self.my_opinion(token, judge_record_id).await?;
        Ok(opinion_satisfaction(&data))
    }

    /// 已判决列表：返回总条数与「案件号 → 判决记录号」映射。
    pub async fn judged_refs(
        &self,
        token: &str,
        page: u32,
        page_size: u32,
    ) -> Result<(i64, Vec<JudgedRef>), ApiError> {
        let data = self.judged_reports(token, page, page_size).await?;
        let total = int(&data["total"]).unwrap_or(0);
        let refs = data["list"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        // 列表项的 id 就是案件号（例如 id=57864 → judgeRecordId=41873）。
                        let case_id = int(&item["id"])
                            .or_else(|| int(&item["caseId"]))
                            .unwrap_or(0);
                        let judge_record_id = int(&item["judgeRecordId"]).unwrap_or(0);
                        // 判决列表自带 opinionInfo，直接拿来算满意度；
                        // 已经投过票的案件再调 myOpinion 会回 107005。
                        let opinion = (item["opinionInfo"].is_object())
                            .then(|| {
                                (
                                    int(&item["opinionInfo"]["userAgreeNum"]).unwrap_or(0),
                                    int(&item["opinionInfo"]["userDisagreeNum"]).unwrap_or(0),
                                )
                            })
                            .filter(|(agree, disagree)| *agree > 0 || *disagree > 0);
                        (case_id > 0).then_some(JudgedRef {
                            case_id,
                            judge_record_id,
                            opinion,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok((total, refs))
    }

    /// 扫描若干页判决列表，返回需要的案件号对应的判决记录号。
    /// 判决列表按判决时间倒序，新提交的案件基本都在第一页。
    pub async fn judged_index(
        &self,
        token: &str,
        wanted: &[i64],
        max_pages: u32,
    ) -> Result<std::collections::HashMap<i64, JudgedRef>, ApiError> {
        let wanted: std::collections::HashSet<i64> =
            wanted.iter().copied().filter(|id| *id > 0).collect();
        let mut found = std::collections::HashMap::new();
        if wanted.is_empty() || max_pages == 0 {
            return Ok(found);
        }
        let mut page = 1;
        loop {
            let (total, refs) = self.judged_refs(token, page, JUDGED_PAGE_SIZE).await?;
            for item in refs {
                if item.judge_record_id > 0 && wanted.contains(&item.case_id) {
                    found.insert(item.case_id, item);
                }
            }
            if found.len() >= wanted.len() || page >= max_pages {
                break;
            }
            let pages = if total > 0 {
                ((total + JUDGED_PAGE_SIZE as i64 - 1) / JUDGED_PAGE_SIZE as i64) as u32
            } else {
                1
            };
            if page >= pages {
                break;
            }
            page += 1;
        }
        Ok(found)
    }

    /// 当前审核员最近判决的一个案件号；用于在已审核列表中反查 checkerId。
    pub async fn latest_judged_case_id(&self, token: &str) -> Result<i64, ApiError> {
        let (_total, refs) = self.judged_refs(token, 1, JUDGED_PAGE_SIZE).await?;
        Ok(refs.first().map(|item| item.case_id).unwrap_or(0))
    }

    /// 按案件号精确找判决记录号；找不到返回 0。
    pub async fn unread(&self, token: &str) -> Result<Value, ApiError> {
        let data = self.call(token, Method::GET, "/message/get-unread", &[], None).await?;
        Ok(json!({ "messageUnread": data["messageUnread"] }))
    }

    /// 待审核列表。`report_id` 有值时按案件号搜索（上游用 reportId 精确过滤），
    /// 此时不带 reportStatus，避免已处理案件被状态条件挡掉。
    pub async fn cases(
        &self,
        token: &str,
        page: u32,
        page_size: u32,
        report_id: Option<i64>,
    ) -> Result<CasePage, ApiError> {
        let mut query = vec![
            ("page", page.to_string()),
            ("pageSize", page_size.to_string()),
        ];
        match report_id {
            Some(id) if id > 0 => query.push(("reportId", id.to_string())),
            _ => query.push(("reportStatus", "0".to_string())),
        }
        let data = self.call(token, Method::GET, "/report/get-comment-list", &query, None).await?;
        let list = data["list"]
            .as_array()
            .map(|items| items.iter().map(CaseSummary::from_value).collect())
            .unwrap_or_default();
        Ok(CasePage {
            total: int(&data["total"]).unwrap_or(0),
            page,
            page_size,
            list,
        })
    }

    /// 已审核案件列表：同一个接口去掉 reportStatus=0。
    /// 返回项的 checkerId 是审核员社区用户 ID，lastReportTime 是 Unix 秒。
    pub async fn audited_cases(
        &self,
        token: &str,
        page: u32,
        page_size: u32,
    ) -> Result<CasePage, ApiError> {
        let query = vec![
            ("page", page.to_string()),
            ("pageSize", page_size.to_string()),
        ];
        let data = self.call(token, Method::GET, "/report/get-comment-list", &query, None).await?;
        let list = data["list"]
            .as_array()
            .map(|items| items.iter().map(CaseSummary::from_value).collect())
            .unwrap_or_default();
        Ok(CasePage {
            total: int(&data["total"]).unwrap_or(0),
            page,
            page_size,
            list,
        })
    }

    /// 作品待审列表：/report/get-composition-list。
    /// 不带 report_id 时加 reportStatus=0，只取未审核作品。
    pub async fn composition_cases(
        &self,
        token: &str,
        page: u32,
        page_size: u32,
        report_id: Option<i64>,
    ) -> Result<CasePage, ApiError> {
        let mut query = vec![
            ("page", page.to_string()),
            ("pageSize", page_size.to_string()),
        ];
        match report_id {
            Some(id) if id > 0 => query.push(("reportId", id.to_string())),
            _ => query.push(("reportStatus", "0".to_string())),
        }
        let data = self
            .call(token, Method::GET, "/report/get-composition-list", &query, None)
            .await?;
        let list = data["list"]
            .as_array()
            .map(|items| items.iter().map(CaseSummary::from_value).collect())
            .unwrap_or_default();
        Ok(CasePage {
            total: int(&data["total"]).unwrap_or(0),
            page,
            page_size,
            list,
        })
    }

    /// 已审核作品列表：同一个接口去掉 reportStatus=0。
    pub async fn audited_composition_cases(
        &self,
        token: &str,
        page: u32,
        page_size: u32,
    ) -> Result<CasePage, ApiError> {
        let query = vec![
            ("page", page.to_string()),
            ("pageSize", page_size.to_string()),
        ];
        let data = self
            .call(token, Method::GET, "/report/get-composition-list", &query, None)
            .await?;
        let list = data["list"]
            .as_array()
            .map(|items| items.iter().map(CaseSummary::from_value).collect())
            .unwrap_or_default();
        Ok(CasePage {
            total: int(&data["total"]).unwrap_or(0),
            page,
            page_size,
            list,
        })
    }

    /// 提交作品审核：/report/verify-composition，请求体与评论审核一致。
    pub async fn verify_composition(
        &self,
        token: &str,
        case: &CaseDetail,
        action: u8,
        remark: &str,
        user_message: &str,
        reporter_message: &str,
    ) -> Result<usize, ApiError> {
        let payload = composition_verify_body(case, action, remark, user_message, reporter_message)?;
        self.call(token, Method::POST, "/report/verify-composition", &[], Some(&payload))
            .await?;
        Ok(1)
    }

    pub async fn case_detail(&self, token: &str, report_id: i64) -> Result<CaseDetail, ApiError> {
        let query = [("reportId", report_id.to_string())];
        let data = self.call(token, Method::GET, "/report/get-info", &query, None).await?;
        Ok(CaseDetail::from_value(&data, report_id))
    }

    pub async fn verify(&self, token: &str, payload: &Value) -> Result<(), ApiError> {
        self.call(token, Method::POST, "/report/verify-comment", &[], Some(payload))
            .await
            .map(|_| ())
    }

    /// 完整受理流程：重新拉详情收集全部举报记录，再提交。
    /// HTTP 处理器和后台自动审核共用，保证两边口径一致。
    /// 返回 (案件详情, 已受理举报条数)，详情给审核记录留档用。
    pub async fn verify_case(
        &self,
        token: &str,
        report_id: i64,
        action: u8,
        remark: &str,
        user_message: &str,
        reporter_message: &str,
    ) -> Result<(CaseDetail, usize), ApiError> {
        let case = self.case_detail(token, report_id).await?;
        let accepted = self
            .verify_report(token, &case, action, remark, user_message, reporter_message)
            .await?;
        Ok((case, accepted))
    }

    /// 用已经拿到的详情直接提交，避免二次请求详情。
    pub async fn verify_report(
        &self,
        token: &str,
        case: &CaseDetail,
        action: u8,
        remark: &str,
        user_message: &str,
        reporter_message: &str,
    ) -> Result<usize, ApiError> {
        let payload = comment_verify_body(case, action, remark, user_message, reporter_message)?;
        self.verify(token, &payload).await?;
        Ok(1)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CasePage {
    pub total: i64,
    pub page: u32,
    pub page_size: u32,
    pub list: Vec<CaseSummary>,
}

/// The six list fields required by the specification; everything else is dropped.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CaseSummary {
    pub id: i64,
    pub user_id: String,
    pub nickname: String,
    pub content: String,
    pub checker_name: String,
    pub composition_id: String,
    /// 举报时间（原始字符串），用于跳过放置过久的案件。
    pub created_at: String,
    /// 审核员社区用户 ID；已审核列表用它匹配当前审核员。
    pub checker_id: i64,
    /// 最后举报时间（Unix 秒），作品列表通常只有这个时间字段。
    pub last_report_time: i64,
    /// 作品标题 / 网页地址 / 封面（评论案件为空）。
    pub title: String,
    pub source_url: String,
    pub cover_url: String,
    pub report_num: i64,
}

/// 列表接口的时间字段名各版本不一，逐个尝试。
fn pick_text(value: &Value, keys: &[&str]) -> String {
    for key in keys {
        let candidate = text(&value[*key]);
        if !candidate.trim().is_empty() {
            return candidate;
        }
    }
    String::new()
}

/// 举报时间字段名。各版本接口不一，逐个尝试。
/// 注意：这里只认"举报时间"，绝不回退到被举报评论的发布时间。
const REPORT_TIME_KEYS: &[&str] = &[
    "reportTimeFormat",
    "createTimeFormat",
    "reportTime",
    "createTime",
    "createAt",
    "gmtCreate",
];

impl CaseSummary {
    fn from_value(value: &Value) -> Self {
        let target = &value["targetObject"];
        // 评论用 fromUserObject；作品用 userObject。
        let author = if target["fromUserObject"].is_object() {
            &target["fromUserObject"]
        } else {
            &target["userObject"]
        };
        let composition = if target["compositionObject"].is_object() {
            &target["compositionObject"]
        } else if target["compositionId"].is_string() {
            target
        } else {
            &value["compositionObject"]
        };
        let mut created_at = pick_text(value, REPORT_TIME_KEYS);
        if created_at.is_empty() {
            // 有些版本把举报信息包在 report 对象里。
            created_at = pick_text(&value["report"], REPORT_TIME_KEYS);
        }
        Self {
            id: int(&value["id"]).unwrap_or(0),
            user_id: text(&author["userId"]),
            nickname: text(&author["nickname"]),
            content: text(&target["content"]),
            checker_name: text(&value["checkerName"]),
            composition_id: text(&composition["compositionId"]),
            created_at,
            checker_id: int(&value["checkerId"]).unwrap_or(0),
            last_report_time: int(&value["lastReportTime"]).unwrap_or(0),
            title: text(&composition["title"]),
            source_url: text(&composition["sourceUrl"]),
            cover_url: text(&composition["coverKey"]),
            report_num: int(&value["reportNum"]).unwrap_or(0),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserBrief {
    pub user_id: String,
    pub nickname: String,
    pub avatar: String,
    pub autograph: String,
}

impl UserBrief {
    fn from_value(value: &Value) -> Self {
        Self {
            user_id: text(&value["userId"]),
            nickname: text(&value["nickname"]),
            avatar: text(&value["avatarImg"]),
            autograph: text(&value["autograph"]),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportRecord {
    pub id: i64,
    pub content: String,
    pub reason_name: String,
    pub images: Vec<String>,
    pub created_at: String,
    /// 这条举报由谁处理。列表接口不返回，只有详情里有。
    pub checker_id: i64,
    pub checker_name: String,
    pub reporter: UserBrief,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Composition {
    pub composition_id: String,
    pub title: String,
    pub description: String,
    pub tags: Vec<String>,
    /// 作品网页地址（作品审核内嵌展示用）。
    pub source_url: String,
    pub cover_url: String,
    pub publish_time: String,
    pub view_count: i64,
    pub like_count: i64,
    pub comment_count: i64,
    pub collect_count: i64,
    pub sb_version: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaseDetail {
    pub report_id: i64,
    pub report_num: i64,
    pub status: i64,
    pub checker_id: i64,
    pub target_type: String,
    pub content: String,
    pub comment_created_at: String,
    pub author: UserBrief,
    pub composition: Composition,
    pub reports: Vec<ReportRecord>,
    pub judge_records: Value,
    pub is_deal: bool,
}

fn image_url(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| ["url", "imgUrl", "src"].iter().find_map(|key| value[*key].as_str().map(str::to_string)))
        .filter(|url| !url.trim().is_empty())
}

/// 受理记录只取最新那一条：get-info 的 list 按举报时间倒序，list[0].id 才是本次要处理的。
fn latest_report_id(case: &CaseDetail) -> Result<i64, ApiError> {
    case.reports
        .first()
        .map(|report| report.id)
        .filter(|id| *id > 0)
        .ok_or_else(|| ApiError::BadRequest("未找到可受理的举报记录".to_string()))
}

/// 评论审核请求体：action 1 = 正常显示（不发站内信），2 = 替换，3 = 删除（发站内信）。
pub(crate) fn comment_verify_body(
    case: &CaseDetail,
    action: u8,
    remark: &str,
    user_message: &str,
    reporter_message: &str,
) -> Result<Value, ApiError> {
    let report_id = latest_report_id(case)?;
    let notify_user = action != 1;
    let mut payload = json!({
        "reportId": case.report_id,
        "reporterIsSend": 1,
        "userIsSend": if notify_user { 1 } else { 0 },
        "reportDetailIds": [report_id],
        "action": action,
        "remark": remark,
        "reporterMessage": reporter_message,
    });
    if notify_user {
        payload["userMessage"] = json!(user_message);
    }
    Ok(payload)
}

/// 作品审核请求体：action 2 = 下架（必须给作者发站内信），3 = 正常显示（不发）。
pub(crate) fn composition_verify_body(
    case: &CaseDetail,
    action: u8,
    remark: &str,
    user_message: &str,
    reporter_message: &str,
) -> Result<Value, ApiError> {
    let report_id = latest_report_id(case)?;
    let notify_user = action == 2;
    let mut payload = json!({
        "reportId": case.report_id,
        "reporterIsSend": 1,
        "userIsSend": if notify_user { 1 } else { 0 },
        "reportDetailIds": [report_id],
        "action": action,
        "remark": remark,
        "reporterMessage": reporter_message,
    });
    if notify_user {
        payload["userMessage"] = json!(user_message);
    }
    Ok(payload)
}

impl CaseDetail {
    /// 列表接口不返回认领信息，只有 get-info 的 list 里有；
    /// 该 list 按举报时间倒序（最新在前），所以第一条的 checkerName 就是
    /// 「这个案件现在归谁」。空串或横线表示还没人认领。
    pub fn latest_checker_name(&self) -> Option<String> {
        self.reports.first().map(|report| report.checker_name.clone())
    }

    /// 最早一条举报的举报时间（原始字符串），用于超期判断与审核记录留档。
    /// 只取举报记录的时间，绝不回退到评论发布时间。
    pub fn earliest_report_time(&self) -> String {
        self.reports
            .iter()
            .map(|report| report.created_at.trim())
            .filter(|value| !value.is_empty())
            .min()
            .unwrap_or("")
            .to_string()
    }

    fn from_value(data: &Value, requested_id: i64) -> Self {
        let report = &data["report"];
        let target = &data["targetObject"];
        // 评论详情的作品在 compositionObject；作品审核详情本身就是作品对象。
        let composition = if target["compositionObject"].is_object() {
            &target["compositionObject"]
        } else if target["compositionId"].is_string() {
            target
        } else {
            &data["compositionObject"]
        };
        let author_value = if target["fromUserObject"].is_object() {
            &target["fromUserObject"]
        } else {
            &target["userObject"]
        };

        let reports: Vec<ReportRecord> = data["list"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| ReportRecord {
                        id: int(&item["id"]).unwrap_or(0),
                        content: text(&item["content"]),
                        reason_name: text(&item["reasonName"]),
                        images: item["reportImgs"]
                            .as_array()
                            .map(|images| images.iter().filter_map(image_url).collect())
                            .unwrap_or_default(),
                        // 举报记录的时间就是"举报时间"，同样不取评论发布时间。
                        created_at: pick_text(item, REPORT_TIME_KEYS),
                        checker_id: int(&item["checkerId"]).unwrap_or(0),
                        checker_name: text(&item["checkerName"]),
                        reporter: UserBrief::from_value(&item["userObject"]),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let judge_records = match &data["judgeRecordObject"] {
            Value::Object(map) if map.is_empty() => Value::Null,
            Value::Array(items) if items.is_empty() => Value::Null,
            other => other.clone(),
        };

        Self {
            report_id: int(&report["id"]).filter(|id| *id > 0).unwrap_or(requested_id),
            report_num: int(&report["reportNum"]).unwrap_or(reports.len() as i64),
            status: int(&report["status"]).unwrap_or(0),
            checker_id: int(&report["checkerId"]).unwrap_or(0),
            target_type: text(&report["targetType"]),
            content: text(&target["content"]),
            comment_created_at: text(&target["createTimeFormat"]),
            author: UserBrief::from_value(author_value),
            composition: Composition {
                composition_id: text(&composition["compositionId"]),
                title: text(&composition["title"]),
                description: text(&composition["description"]),
                tags: composition["tags"]
                    .as_array()
                    .map(|tags| {
                        tags.iter()
                            .map(|tag| {
                                let name = text(&tag["tagName"]);
                                if name.is_empty() {
                                    text(tag)
                                } else {
                                    name
                                }
                            })
                            .filter(|name| !name.is_empty())
                            .collect()
                    })
                    .unwrap_or_default(),
                source_url: text(&composition["sourceUrl"]),
                cover_url: text(&composition["coverKey"]),
                publish_time: pick_text(
                    composition,
                    &["publishTimeFormat", "createTimeFormat", "saveTimeFormat"],
                ),
                view_count: int(&composition["statObject"]["viewCount"]).unwrap_or(0),
                like_count: int(&composition["statObject"]["likeCount"]).unwrap_or(0),
                comment_count: int(&composition["statObject"]["commentCount"]).unwrap_or(0),
                collect_count: int(&composition["statObject"]["collectCount"]).unwrap_or(0),
                sb_version: int(&composition["sbVersion"]).unwrap_or(0),
            },
            reports,
            judge_records,
            is_deal: int(&data["isDeal"]).unwrap_or(0) != 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn satisfaction_drops_the_reviewers_own_vote() {
        // 10 赞同 = 审核员自己 1 票 + 其他用户 9 票，反对 0 → 100%。
        let value = json!({ "userAgreeNum": 10, "userDisagreeNum": 0, "userAgreeRatio": 100 });
        assert_eq!(opinion_satisfaction(&value), Some(1.0));
        // 6 赞同（其他 5）+ 4 反对 → 5/9。
        let value = json!({ "userAgreeNum": 6, "userDisagreeNum": 4 });
        assert_eq!(opinion_satisfaction(&value), Some(5.0 / 9.0));
        // 只有审核员自己投过票 → 还没有满意度。
        let value = json!({ "userAgreeNum": 1, "userDisagreeNum": 0 });
        assert_eq!(opinion_satisfaction(&value), None);
        // 没人赞同、只有反对 → 0%。
        let value = json!({ "userAgreeNum": 1, "userDisagreeNum": 3 });
        assert_eq!(opinion_satisfaction(&value), Some(0.0));
        assert_eq!(opinion_satisfaction(&Value::Null), None);
    }

    #[test]
    fn judged_list_counts_give_satisfaction() {
        // 与线上一致：userAgreeNum 含审核员自己那一票（5 → 其他 4 票赞同），反对 1。
        assert_eq!(satisfaction_from_counts(5, 1), Some(0.8));
        // 只有审核员自己投过票 → 还没有满意度。
        assert_eq!(satisfaction_from_counts(1, 0), None);
        assert_eq!(satisfaction_from_counts(0, 0), None);
        assert_eq!(satisfaction_from_counts(0, 3), Some(0.0));
    }

    #[test]
    fn latest_report_decides_claim() {
        // 真实形状：多条举报按时间倒序，最新那条 checkerName 为 "--"，
        // 更早那条已经被同一个人处理过。认领状态只看最新的这条。
        let detail = json!({
            "list": [
                { "id": 80443, "createTimeFormat": "2026-11-01 07:56", "checkerId": 0, "checkerName": "--" },
                { "id": 80442, "createTimeFormat": "2026-10-31 23:42", "checkerId": 3369552, "checkerName": "Hagkiest" }
            ],
            "report": { "id": 57885, "targetType": "compose", "reportNum": 2, "lastReportTime": 1793490993 }
        });
        let parsed = CaseDetail::from_value(&detail, 57885);
        assert_eq!(parsed.latest_checker_name().as_deref(), Some("--"));
        assert_eq!(parsed.report_num, 2);
        assert!(crate::auto::unclaimed(&parsed.latest_checker_name().unwrap()));
    }
    fn sample_case() -> CaseDetail {
        // 与线上一致：list 按时间倒序，最新一条未认领，旧的一条已被处理。
        let detail = json!({
            "list": [
                { "id": 80445, "createTimeFormat": "2026-11-01 09:10", "checkerId": 0, "checkerName": "--" },
                { "id": 80443, "createTimeFormat": "2026-11-01 07:56", "checkerId": 3369552, "checkerName": "Hagkiest" }
            ],
            "report": { "id": 57886, "targetType": "compose", "reportNum": 2, "lastReportTime": 1793491860 }
        });
        CaseDetail::from_value(&detail, 57886)
    }

    #[test]
    fn composition_body_matches_official_shape() {
        let case = sample_case();
        // 正常显示：action=3，不发站内信，也没有 userMessage 字段。
        let body = composition_verify_body(&case, 3, "无违规", "", "感谢您对社区内容的正义维护，您的举报已受理。").unwrap();
        assert_eq!(body["action"], 3);
        assert_eq!(body["userIsSend"], 0);
        assert!(body.get("userMessage").is_none());
        assert_eq!(body["reportDetailIds"], json!([80445]));

        // 下架：action=2，必须带 userIsSend=1 和 userMessage，且只受理最新那条举报。
        let body = composition_verify_body(&case, 2, "人身攻击", "您的作品存在违规内容", "感谢您对社区内容的正义维护，您的举报已受理。").unwrap();
        assert_eq!(body["action"], 2);
        assert_eq!(body["userIsSend"], 1);
        assert_eq!(body["userMessage"], "您的作品存在违规内容");
        assert_eq!(body["reportDetailIds"], json!([80445]));
    }

    #[test]
    fn comment_body_omits_user_message_for_normal_display() {
        let case = sample_case();
        let body = comment_verify_body(&case, 1, "未发现违规", "不该出现", "感谢您对社区内容的正义维护，经核查该内容未发现违规。").unwrap();
        assert_eq!(body["userIsSend"], 0);
        assert!(body.get("userMessage").is_none());
        assert_eq!(body["reportDetailIds"], json!([80445]));

        let body = comment_verify_body(&case, 3, "辱骂", "您的评论已被删除", "感谢您对社区内容的正义维护，您的举报已受理。").unwrap();
        assert_eq!(body["userIsSend"], 1);
        assert_eq!(body["userMessage"], "您的评论已被删除");
    }

}

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use reqwest::Client;
use serde::Serialize;
use serde_json::{json, Value};

use crate::{
    community::{int, text, CaseDetail},
    error::ApiError,
};

const COMMUNITY_RULES: &str = "社区规范要点（摘自小码王社区规范 4.0）：\n\
1. 九不准：违法、危害国家安全、破坏民族团结、宣扬邪教迷信、散布谣言、淫秽色情赌博暴力恐怖、侮辱诽谤他人等；\n\
2. 侵犯他人权益：辱骂、人身攻击、言语骚扰、嘲讽挑拨引战、泄露他人隐私、未经许可搬运他人作品；\n\
3. 违规宣传：拉群、留联系方式、工作室广告邀请、垃圾广告与恶意营销；\n\
4. 垃圾内容：无意义文字超过 3 行、表情包超过 3 行、刷屏并影响他人体验；\n\
5. 未经允许公布存档码属于违规；\n\
6. 露骨恋爱内容、消极厌学弃学内容、账号或金币交易同样违规。\n\
处罚原则：初次违规且情节较轻，以提醒、删评为主。";

const SYSTEM_PROMPT: &str = "你是小码王社区的风纪审核助手，负责根据社区规范对被举报的评论给出审核结论。\
只依据被举报评论的正文判断，不要考虑举报理由、举报人、作品信息和作者身份。\
每次判定都必须调用一次 submit_review 工具提交结果，不要直接输出普通文本。\n\
链接判定（必须遵守）：\n\
1. 评论中出现任何指向小码王以外网站（含短链、IP 地址、裸域名）的链接，一律视为广告引流，action=3；\n\
2. 小码王站内链接（xiaomawang.com 及其子域）要看用途：推荐自己或他人作品、刷热度、拉人加群加好友、求关注点赞等以引流为目的，属广告引流，action=3；\n\
3. 获奖公示或获奖作品链接、官方活动页、公告、教程帮助、必要的出处引用等有正当理由的小码王链接，action=1，不要删除；\n\
4. 举报理由只针对链接时，链接属于第 3 条正当情形就判 action=1；只有链接指向站外才判 action=3。\n\
受理结果的判定标准：\n\
action=3（删除）：散播存档码/外挂/修改器、广告或引流（互关、加群、留联系方式）、辱骂攻击他人、色情暴力等明确违规内容；\n\
注意：“乐子”“乐乐”“sb”“78”“91”等文本属于违规；\n\
action=2（替换）：只有局部内容违规、替换后可保留；删除与替换都适用时优先选择删除（action=3）；\n\
action=1（正常显示）：正常的作品交流、提问、催更、善意调侃，或举报理由不成立。\n\
评论中的 emoji、特殊符号和长数字串都应按原文理解。\n\
自动审核时，举报时间超过一周（7 天）的案件会直接跳过。";

fn system_prompt() -> String {
    format!("{SYSTEM_PROMPT}\n审核依据：\n{COMMUNITY_RULES}")
}

/// 模型/网关不支持 tools 时的兜底提示：只多一句 JSON 字段说明，规则与主提示一致。
fn legacy_system_prompt() -> String {
    format!(
        "{}\n无法调用工具时，只返回一个 JSON 对象，不要包含其他文字。字段：action（1 = 正常显示，2 = 替换，3 = 删除）、violationType、remark、reason、confidence（0 到 1 的小数）。",
        system_prompt()
    )
}

/// 受理备注是公示内容，统一限制长度并去掉首尾标点。
const REMARK_LIMIT: usize = 20;

/// 当前模型/网关不支持工具调用时置位，进程内后续请求直接走 JSON，避免每次都多打一次。
static TOOLS_UNSUPPORTED: AtomicBool = AtomicBool::new(false);

/// 提交审核的工具定义：字段含义与写法都放在这里，系统提示词只保留审核规则和调用方式。
fn submit_review_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "submit_review",
            "description": "提交本次举报案件的审核结论，每次判定必须调用且只调用一次。remark 会公示到公告栏：用一句话写违规原因，不超过 20 字，不要写时间、天数、处理过程或复述举报理由。",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": {
                        "type": "integer",
                        "enum": [1, 2, 3],
                        "description": "受理结果：1 = 正常显示，2 = 替换，3 = 删除；删除与替换都适用时选 3。"
                    },
                    "violationType": {
                        "type": "string",
                        "description": "违规类型，例如“广告引流”“散播存档码”“辱骂攻击”；未违规填“无违规”。"
                    },
                    "remark": {
                        "type": "string",
                        "description": "受理备注（公示）：一句话说明违规原因，不超过 20 字，客观简洁；未违规填“未发现违规”。"
                    },
                    "reason": {
                        "type": "string",
                        "description": "给审核员参考的判定依据，可稍长。"
                    },
                    "confidence": {
                        "type": "number",
                        "description": "0 到 1 的小数，表示判定的把握程度；证据不足或上下文不确定时低于 0.7。"
                    }
                },
                "required": ["action", "violationType", "remark", "reason", "confidence"],
                "additionalProperties": false
            }
        }
    })
}

/// 单次 AI 请求的上限。旧实现在这里用 90 秒，坏密钥会把一个审核周期整个拖死。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(25);

/// 官方站内信文案：由代码按 action 固定生成，不再让模型自由发挥（后台自动审核也复用）。
pub const REPORTER_MESSAGE: &str = "感谢您对社区内容的正义维护，您的举报已受理。";
pub const USER_REPLACE_MESSAGE: &str = "您的评论存在违规内容，已被系统管理员删除。";
pub const USER_DELETE_MESSAGE: &str = "您的评论存在违规内容，该评论及子回复已被系统管理员删除。";
pub const REPORTER_NORMAL_MESSAGE: &str = "感谢您对社区内容的正义维护，经核查该内容未发现违规。";
pub const USER_NORMAL_MESSAGE: &str = "经核查，您的评论未发现违规，已恢复正常显示。";

/// 混淆用的固定密钥，必须与 scripts/build.ps1 中的 XOR 常量保持一致。
const OBF_KEY: &[u8] = b"qingfeng-review-2026-key-obfuscation";

mod builtin {
    include!(concat!(env!("OUT_DIR"), "/builtin_keys.rs"));
}

/// Base64（含 URL-safe 变体）解码，JWT payload 也用它。
pub(crate) fn b64_decode(input: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        } as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    out
}

/// 解开打包时写入的内置配置（XOR + Base64），文件中看不到明文。
pub(crate) fn deobfuscate(encoded: &str) -> String {
    let trimmed = encoded.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let decoded = b64_decode(trimmed);
    let bytes: Vec<u8> = decoded
        .into_iter()
        .enumerate()
        .map(|(index, byte)| byte ^ OBF_KEY[index % OBF_KEY.len()])
        .collect();
    String::from_utf8_lossy(&bytes).trim().to_string()
}

fn split_keys(raw: &str) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for part in raw.split(|character: char| character == ',' || character == ';' || character.is_whitespace()) {
        let key = part.trim();
        if !key.is_empty() && !keys.iter().any(|existing| existing == key) {
            keys.push(key.to_string());
        }
    }
    keys
}

/// 运行时可配置的密钥变量，按顺序拼接：AI_API_KEY、AI_API_KEY_1..9、AI_API_KEY_FALLBACK、AI_API_KEYS。
fn env_keys() -> Vec<String> {
    let mut names = vec!["AI_API_KEY".to_string()];
    for index in 1..=9 {
        names.push(format!("AI_API_KEY_{index}"));
    }
    names.push("AI_API_KEY_FALLBACK".to_string());
    names.push("AI_API_KEYS".to_string());

    let mut raw = String::new();
    for name in names {
        if let Ok(value) = std::env::var(&name) {
            raw.push(',');
            raw.push_str(&value);
        }
    }
    split_keys(&raw)
}

/// 受理结果既可能是数字，也可能被模型写成“删除 / 替换 / 正常显示”。
fn action_of(value: &Value) -> Option<i64> {
    if let Some(action) = int(value).filter(|action| (1..=3).contains(action)) {
        return Some(action);
    }
    let label = text(value);
    let label = label.trim();
    if label.contains("删除") || label.eq_ignore_ascii_case("delete") {
        Some(3)
    } else if label.contains("替换")
        || label.contains("屏蔽")
        || label.eq_ignore_ascii_case("replace")
    {
        Some(2)
    } else if label.contains("正常") || label.eq_ignore_ascii_case("keep") || label.eq_ignore_ascii_case("none") {
        Some(1)
    } else {
        // 可能是 "2（替换）"、3.0 这类写法，取开头的数字。
        label
            .chars()
            .take_while(|character| character.is_ascii_digit())
            .collect::<String>()
            .parse()
            .ok()
            .filter(|action| (1..=3).contains(action))
    }
}

fn mask(key: &str) -> String {    let visible: Vec<char> = key.chars().collect();
    if visible.len() <= 10 {
        return "****".to_string();
    }
    let head: String = visible.iter().take(6).collect();
    let tail: String = visible.iter().skip(visible.len() - 4).collect();
    format!("{head}…{tail}")
}

/// 每个密钥每分钟最多 10 次请求（上游配额），超出的请求换下一个密钥。
const KEY_RATE_LIMIT: u32 = 10;
const KEY_RATE_WINDOW: Duration = Duration::from_secs(60);

struct KeySlot {
    key: String,
    masked: String,
    failures: u32,
    cooldown_until: Option<Instant>,
    last_error: String,
    window_start: Instant,
    window_used: u32,
}

impl KeySlot {
    fn new(key: String) -> Self {
        Self {
            masked: mask(&key),
            key,
            failures: 0,
            cooldown_until: None,
            last_error: String::new(),
            window_start: Instant::now(),
            window_used: 0,
        }
    }

    fn cooling(&self, now: Instant) -> bool {
        self.cooldown_until.map(|until| until > now).unwrap_or(false)
    }

    fn cooldown_secs(&self, now: Instant) -> u64 {
        self.cooldown_until
            .filter(|until| *until > now)
            .map(|until| until.duration_since(now).as_secs().max(1))
            .unwrap_or(0)
    }

    /// 本分钟还能发多少次请求。
    fn remaining(&self, now: Instant) -> u32 {
        if now.duration_since(self.window_start) >= KEY_RATE_WINDOW {
            KEY_RATE_LIMIT
        } else {
            KEY_RATE_LIMIT.saturating_sub(self.window_used)
        }
    }

    fn available(&self, now: Instant) -> bool {
        !self.cooling(now) && self.remaining(now) > 0
    }

    /// 发请求前先占一个配额；失败的请求同样计入。
    fn take(&mut self, now: Instant) {
        if now.duration_since(self.window_start) >= KEY_RATE_WINDOW {
            self.window_start = now;
            self.window_used = 0;
        }
        self.window_used += 1;
    }
}

struct Inner {
    base_url: String,
    model: String,
    key_source: &'static str,
    slots: Mutex<Vec<KeySlot>>,
}

#[derive(Clone)]
pub struct AiConfig {
    inner: Arc<Inner>,
}

impl AiConfig {
    pub fn from_env() -> Option<Self> {
        let read_env = |key: &str| {
            std::env::var(key)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        let builtin = |obf: &str| {
            let value = deobfuscate(obf);
            if value.is_empty() {
                None
            } else {
                Some(value)
            }
        };

        let (keys, key_source): (Vec<String>, &'static str) = match env_keys() {
            keys if !keys.is_empty() => (keys, "env"),
            _ => (split_keys(&deobfuscate(builtin::BUILTIN_KEYS_OBF)), "builtin"),
        };
        if keys.is_empty() {
            return None;
        }

        let base_url = read_env("AI_BASE_URL")
            .or_else(|| builtin(builtin::BUILTIN_BASE_URL_OBF))?;
        let model = read_env("AI_MODEL").or_else(|| builtin(builtin::BUILTIN_MODEL_OBF))?;

        Some(Self {
            inner: Arc::new(Inner {
                base_url: base_url.trim_end_matches('/').to_string(),
                model,
                key_source,
                slots: Mutex::new(keys.into_iter().map(KeySlot::new).collect()),
            }),
        })
    }

    fn slots(&self) -> std::sync::MutexGuard<'_, Vec<KeySlot>> {
        self.inner.slots.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// 只返回当前可用的密钥（未冷却、本分钟还有配额），保持原顺序。
    pub fn order(&self) -> Vec<usize> {
        let now = Instant::now();
        let slots = self.slots();
        slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.available(now))
            .map(|(index, _)| index)
            .collect()
    }

    pub fn status(&self) -> Value {
        let now = Instant::now();
        let slots = self.slots();
        let keys: Vec<Value> = slots
            .iter()
            .enumerate()
            .map(|(index, slot)| {
                let cooldown = slot.cooldown_secs(now);
                let remaining = slot.remaining(now);
                json!({
                    "index": index + 1,
                    "masked": slot.masked,
                    "state": if cooldown > 0 {
                        "cooldown"
                    } else if remaining == 0 {
                        "throttled"
                    } else {
                        "healthy"
                    },
                    "cooldownSecs": cooldown,
                    "remaining": remaining,
                    "rateLimit": KEY_RATE_LIMIT,
                    "failures": slot.failures,
                    "lastError": slot.last_error,
                })
            })
            .collect();
        json!({
            "enabled": true,
            "model": self.inner.model,
            "keySource": self.inner.key_source,
            "keys": keys,
        })
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Suggestion {
    pub action: i64,
    pub violation_type: String,
    pub remark: String,
    pub user_message: String,
    pub reporter_message: String,
    pub reason: String,
    pub confidence: f64,
}

/// Only the reported comment text is sent to the model; everything else is noise.
pub fn build_context(case: &CaseDetail) -> String {
    format!("被举报评论：{}", case.content)
}

/// 站外链接强判：模型偶尔会漏看链接，这里用域名规则兜底。
/// 规则：评论里出现任何非 xiaomawang.com 及其子域的链接 → 一律按广告引流删除。
const ALLOWED_DOMAIN: &str = "xiaomawang.com";

fn is_xiaomawang(host: &str) -> bool {
    host == ALLOWED_DOMAIN || host.ends_with(&format!(".{ALLOWED_DOMAIN}"))
}

/// 从文本里粗提域名（支持带协议、裸域名、IP 和短链）。
fn extract_domains(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let separators = " \t\r\n，。、；：！？（）【】《》〈〉“”‘’\"'<>|\\";
    for raw in text.split(|c: char| separators.contains(c)) {
        let token = raw.trim_matches(|c: char| ".,;:!?()[]{}<>\"'`·…".contains(c));
        if token.len() < 4 {
            continue;
        }
        let mut host = token.to_ascii_lowercase();
        for prefix in ["https://", "http://", "//"] {
            if let Some(rest) = host.strip_prefix(prefix) {
                host = rest.to_string();
            }
        }
        // 去掉路径、查询、锚点和端口
        host = host
            .split(['/', '?', '#'])
            .next()
            .unwrap_or("")
            .split(':')
            .next()
            .unwrap_or("")
            .trim_start_matches("www.")
            .trim_end_matches('.')
            .to_string();
        if host.is_empty() || !host.contains('.') {
            continue;
        }
        let labels: Vec<&str> = host.split('.').collect();
        if labels.iter().any(|label| label.is_empty()) {
            continue;
        }
        let tld = labels.last().copied().unwrap_or("");
        let is_domain = tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic());
        let is_ip = labels.len() == 4 && labels.iter().all(|l| l.chars().all(|c| c.is_ascii_digit()));
        if !(is_domain || is_ip) {
            continue;
        }
        if !found.iter().any(|existing| existing == &host) {
            found.push(host);
        }
    }
    found
}

/// 返回评论里第一个站外域名。
fn external_domain(text: &str) -> Option<String> {
    extract_domains(text)
        .into_iter()
        .find(|host| !is_xiaomawang(host))
}

/// 命中站外链接就强制改成删除，覆盖模型给出的 action。
fn apply_link_policy(case: &CaseDetail, suggestion: &mut Suggestion) {
    let Some(domain) = external_domain(&case.content) else {
        return;
    };
    if suggestion.action != 3 {
        crate::log(format!(
            "[link] 评论含站外链接 {domain}，按广告引流强制删除（原判定 action={}）",
            suggestion.action
        ));
    }
    suggestion.action = 3;
    suggestion.violation_type = "广告引流".to_string();
    suggestion.remark = "评论含站外链接，属广告引流".to_string();
    suggestion.user_message = USER_DELETE_MESSAGE.to_string();
    if suggestion.reporter_message.is_empty() {
        suggestion.reporter_message = REPORTER_MESSAGE.to_string();
    }
    suggestion.confidence = suggestion.confidence.max(0.95);
    suggestion.reason = format!("评论包含指向小码王以外网站（{domain}）的链接，按规则一律视为广告引流。");
}

struct AttemptError {
    error: ApiError,
    /// 需要把这个密钥打入冷宫多久；None 表示问题不在密钥上（例如模型输出格式错误）。
    cooldown: Option<Duration>,
    /// 网关拒绝工具参数：同一个密钥改用 JSON 模式再试一次。
    retry_json: bool,
}

pub async fn suggest(http: &Client, config: &AiConfig, case: &CaseDetail) -> Result<Suggestion, ApiError> {
    let order = config.order();
    if order.is_empty() {
        return Err(ApiError::Ai(
            "所有 AI 密钥都已达到每分钟请求上限或处于冷却中，稍后自动重试".to_string(),
        ));
    }

    let mut last_error = ApiError::Ai("AI 服务不可用".to_string());
    for (position, index) in order.iter().copied().enumerate() {
        // 取密钥的同时占用本分钟配额，避免并发时超发。
        let (key, masked) = {
            let mut slots = config.slots();
            let now = Instant::now();
            match slots.get_mut(index) {
                Some(slot) if slot.available(now) => {
                    slot.take(now);
                    (slot.key.clone(), slot.masked.clone())
                }
                _ => continue,
            }
        };

        match request_suggestion(http, config, &key, case).await {
            Ok(mut suggestion) => {
                {
                    let mut slots = config.slots();
                    if let Some(slot) = slots.get_mut(index) {
                        slot.failures = 0;
                        slot.cooldown_until = None;
                        slot.last_error.clear();
                    }
                }
                if position > 0 {
                    crate::log(format!(
                        "AI 密钥 #{}（{masked}）接管本次判定（第 {} 个可用密钥）",
                        index + 1,
                        position + 1
                    ));
                }
                // 模型可能漏看链接，这里用域名规则兜底覆盖判定。
                apply_link_policy(case, &mut suggestion);
                return Ok(suggestion);
            }
            Err(attempt) => {
                let cooldown = attempt.cooldown;
                let mut slots = config.slots();
                if let Some(slot) = slots.get_mut(index) {
                    slot.failures += 1;
                    slot.last_error = attempt.error.to_string();
                    if let Some(span) = cooldown {
                        slot.cooldown_until = Some(Instant::now() + span);
                    }
                }
                drop(slots);
                crate::log(format!(
                    "AI 密钥 #{}（{masked}）调用失败：{}；{}",
                    index + 1,
                    attempt.error,
                    match cooldown {
                        Some(span) => format!("暂停使用 {} 秒", span.as_secs()),
                        None => "换下一个密钥重试".to_string(),
                    }
                ));
                last_error = attempt.error;
            }
        }
    }
    Err(last_error)
}

fn messages(system: String, case: &CaseDetail) -> Value {
    json!([
        { "role": "system", "content": system },
        { "role": "user", "content": build_context(case) },
    ])
}

/// 首选：让模型调用 submit_review 工具，提交字段与写法都由工具定义约束。
fn tool_body(config: &AiConfig, case: &CaseDetail) -> Value {
    json!({
        "model": config.inner.model,
        "temperature": 0.2,
        "messages": messages(system_prompt(), case),
        "tools": [submit_review_tool()],
        "tool_choice": "auto",
    })
}

/// 兜底：网关/模型不支持 tools 时，退回 response_format=json_object 的纯 JSON 输出。
fn legacy_body(config: &AiConfig, case: &CaseDetail) -> Value {
    json!({
        "model": config.inner.model,
        "temperature": 0.2,
        "response_format": { "type": "json_object" },
        "messages": messages(legacy_system_prompt(), case),
    })
}

async fn request_suggestion(
    http: &Client,
    config: &AiConfig,
    api_key: &str,
    case: &CaseDetail,
) -> Result<Suggestion, AttemptError> {
    if !TOOLS_UNSUPPORTED.load(Ordering::Relaxed) {
        match send(http, config, api_key, &tool_body(config, case)).await {
            Ok(payload) => {
                if let Some(suggestion) = parse_tool_reply(&payload) {
                    return Ok(suggestion);
                }
                crate::log("AI 未返回 submit_review 工具调用，已自动改用 JSON 模式");
                TOOLS_UNSUPPORTED.store(true, Ordering::Relaxed);
            }
            Err(attempt) if attempt.retry_json => {
                crate::log(format!(
                    "AI 服务不支持 submit_review 工具（{}），已自动改用 JSON 模式",
                    attempt.error
                ));
                TOOLS_UNSUPPORTED.store(true, Ordering::Relaxed);
            }
            Err(attempt) => return Err(attempt),
        }
    }

    let payload = send(http, config, api_key, &legacy_body(config, case)).await?;
    parse_content_reply(&payload).map_err(|error| AttemptError {
        error,
        cooldown: None,
        retry_json: false,
    })
}

async fn send(
    http: &Client,
    config: &AiConfig,
    api_key: &str,
    body: &Value,
) -> Result<Value, AttemptError> {
    let response = http
        .post(format!("{}/chat/completions", config.inner.base_url))
        .timeout(REQUEST_TIMEOUT)
        .bearer_auth(api_key)
        .json(body)
        .send()
        .await
        .map_err(|error| AttemptError {
            error: ApiError::Ai(format!("无法连接 AI 服务：{error}")),
            cooldown: Some(Duration::from_secs(15)),
            retry_json: false,
        })?;

    let status = response.status();
    let raw = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let reason = error_message(&raw);
        let cooldown = match status.as_u16() {
            401 | 403 => Duration::from_secs(1800),
            429 => Duration::from_secs(90),
            _ => Duration::from_secs(30),
        };
        return Err(AttemptError {
            error: ApiError::Ai(format!("AI 服务返回 {status}：{reason}")),
            cooldown: Some(cooldown),
            // 请求参数类错误多半是网关不认 tools，换 JSON 再试一次。
            retry_json: matches!(status.as_u16(), 400 | 404 | 422 | 501),
        });
    }

    serde_json::from_str(&raw).map_err(|error| AttemptError {
        error: ApiError::Ai(format!("AI 响应无法解析：{error}")),
        cooldown: Some(Duration::from_secs(15)),
        retry_json: false,
    })
}

/// 工具调用和“工具模式下模型直接回了 JSON”两种响应都接受。
fn parse_tool_reply(payload: &Value) -> Option<Suggestion> {
    if let Some(arguments) = tool_arguments(payload) {
        if let Ok(suggestion) = suggestion_from_value(&arguments) {
            return Some(suggestion);
        }
    }
    let content = payload["choices"][0]["message"]["content"].as_str()?;
    parse_suggestion(content).ok()
}

fn parse_content_reply(payload: &Value) -> Result<Suggestion, ApiError> {
    let content = payload["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| ApiError::Ai("AI 响应为空".to_string()))?;
    parse_suggestion(content)
}

fn tool_arguments(payload: &Value) -> Option<Value> {
    let message = &payload["choices"][0]["message"];
    let raw = if let Some(calls) = message["tool_calls"].as_array() {
        let call = calls
            .iter()
            .find(|call| call["function"]["name"].as_str() == Some("submit_review"))
            .or_else(|| calls.first())?;
        call["function"]["arguments"].clone()
    } else {
        message["function_call"]["arguments"].clone()
    };
    match raw {
        Value::String(text) => parse_json_text(&text),
        Value::Object(_) => Some(raw),
        _ => None,
    }
}

fn parse_json_text(text: &str) -> Option<Value> {
    if let Ok(value) = serde_json::from_str(text.trim()) {
        return Some(value);
    }
    // Some models wrap JSON in prose or code fences; keep only the object.
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str(&text[start..=end]).ok()
}

/// 官方站内信按最终 action 固定生成，同一结果在任何模型下文案一致。
fn official_messages(action: i64) -> (&'static str, &'static str) {
    // (被举报人站内信, 举报人站内信)
    match action {
        1 => (USER_NORMAL_MESSAGE, REPORTER_NORMAL_MESSAGE),
        2 => (USER_REPLACE_MESSAGE, REPORTER_MESSAGE),
        _ => (USER_DELETE_MESSAGE, REPORTER_MESSAGE),
    }
}

/// 受理备注统一口径：压缩空白、去掉首尾标点、最多 20 字；缺失时按类型/结果补齐。
fn normalize_remark(raw: &str, violation_type: &str, action: i64) -> String {
    let trimmed = raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|character: char| "。.;,，；：:!！?？\"'“”‘’".contains(character))
        .to_string();

    let remark = if !trimmed.is_empty() {
        trimmed
    } else if action == 1 || violation_type == "无违规" {
        "未发现违规".to_string()
    } else if violation_type.is_empty() {
        "违反社区规范".to_string()
    } else {
        violation_type.to_string()
    };

    let characters: Vec<char> = remark.chars().collect();
    if characters.len() > REMARK_LIMIT {
        characters[..REMARK_LIMIT].iter().collect()
    } else {
        remark
    }
}

fn suggestion_from_value(value: &Value) -> Result<Suggestion, ApiError> {
    let action = action_of(&value["action"])
        .filter(|action| (1..=3).contains(action))
        .ok_or_else(|| {
            ApiError::Ai(format!(
                "AI 返回的受理结果无效（{}）",
                text(&value["action"])
            ))
        })?;
    let field = |key: &str| text(&value[key]).trim().to_string();
    let confidence = value["confidence"]
        .as_f64()
        .or_else(|| field("confidence").parse().ok())
        .unwrap_or(0.5)
        .clamp(0.0, 1.0);

    let violation_type = field("violationType");
    let remark = normalize_remark(&field("remark"), &violation_type, action);
    let (user_message, reporter_message) = official_messages(action);
    Ok(Suggestion {
        action,
        violation_type,
        remark,
        user_message: user_message.to_string(),
        reporter_message: reporter_message.to_string(),
        reason: field("reason"),
        confidence,
    })
}

fn parse_suggestion(content: &str) -> Result<Suggestion, ApiError> {
    let value = parse_json_text(content)
        .ok_or_else(|| ApiError::Ai("AI 未返回 JSON 格式的建议".to_string()))?;
    suggestion_from_value(&value)
}

/// 上游错误体可能很长，日志里只留一句人话。
fn error_message(raw: &str) -> String {
    if let Ok(payload) = serde_json::from_str::<Value>(raw) {
        for pointer in ["/error/message", "/message", "/error/code"] {
            if let Some(message) = payload.pointer(pointer).and_then(Value::as_str) {
                if !message.trim().is_empty() {
                    return message.trim().to_string();
                }
            }
        }
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "未知错误".to_string();
    }
    trimmed.chars().take(160).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(keys: &[&str]) -> AiConfig {
        AiConfig {
            inner: Arc::new(Inner {
                base_url: "https://example.invalid/v1".to_string(),
                model: "test".to_string(),
                key_source: "test",
                slots: Mutex::new(keys.iter().map(|key| KeySlot::new((*key).to_string())).collect()),
            }),
        }
    }

    #[test]
    fn mask_keeps_only_edges() {
        assert_eq!(mask("sk-1234567890abcdef"), "sk-123…cdef");
        assert_eq!(mask("short"), "****");
    }

    #[test]
    fn cooling_keys_are_skipped() {
        let config = config(&["key-aaaaaaaaaa", "key-bbbbbbbbbb"]);
        assert_eq!(config.order(), vec![0, 1]);

        {
            let mut slots = config.slots();
            slots[0].cooldown_until = Some(Instant::now() + Duration::from_secs(60));
        }
        // 第一把冷却中，直接用第二把。
        assert_eq!(config.order(), vec![1]);
    }

    #[test]
    fn each_key_is_capped_at_ten_requests_per_minute() {
        let config = config(&["key-aaaaaaaaaa", "key-bbbbbbbbbb"]);
        let now = Instant::now();
        {
            let mut slots = config.slots();
            for _ in 0..KEY_RATE_LIMIT {
                slots[0].take(now);
            }
            assert_eq!(slots[0].remaining(now), 0);
            assert!(!slots[0].available(now));
        }
        // 第一把用满 10 次后自动换第二把。
        assert_eq!(config.order(), vec![1]);

        // 过了窗口期配额恢复。
        {
            let mut slots = config.slots();
            slots[0].take(now + KEY_RATE_WINDOW + Duration::from_secs(1));
            assert_eq!(slots[0].remaining(now + KEY_RATE_WINDOW + Duration::from_secs(1)), KEY_RATE_LIMIT - 1);
        }
    }

    #[test]
    fn status_reports_masked_keys_only() {
        let config = config(&["key-aaaaaaaaaa"]);
        let status = config.status();
        assert_eq!(status["keySource"], "test");
        assert_eq!(status["keys"][0]["state"], "healthy");
        assert_eq!(status["keys"][0]["masked"], "key-aa…aaaa");
        assert!(!status.to_string().contains("key-aaaaaaaaaa"));
    }

    #[test]
    fn detects_external_links_only() {
        // 站外链接一律识别出来
        assert_eq!(
            external_domain("看这个 https://www.example.com/abc?id=1 超好玩"),
            Some("example.com".to_string())
        );
        assert_eq!(
            external_domain("加我 8x8x.top 有惊喜"),
            Some("8x8x.top".to_string())
        );
        assert_eq!(
            external_domain("下载地址 192.168.1.10"),
            Some("192.168.1.10".to_string())
        );

        // 小码王站内链接不算站外
        assert_eq!(external_domain("获奖公示 https://world.xiaomawang.com/w/award/1"), None);
        assert_eq!(external_domain("看我的作品 xiaomawang.com/x"), None);

        // 存档码这类长数字串不能被误判成域名
        assert_eq!(
            external_domain("免费送存档码 8837 2910 4471 2209 快进游戏"),
            None
        );
        assert_eq!(external_domain("这个关卡好有创意！"), None);
    }

    #[test]
    fn external_link_forces_delete() {
        let case = CaseDetail {
            report_id: 1,
            report_num: 1,
            status: 0,
            checker_id: 0,
            target_type: "comment".to_string(),
            content: "看我主页 https://spam.example.net 领福利".to_string(),
            comment_created_at: String::new(),
            author: crate::community::UserBrief {
                user_id: String::new(),
                nickname: String::new(),
                avatar: String::new(),
                autograph: String::new(),
            },
            composition: crate::community::Composition {
                composition_id: String::new(),
                title: String::new(),
                description: String::new(),
                tags: Vec::new(),
                source_url: String::new(),
                cover_url: String::new(),
                publish_time: String::new(),
                view_count: 0,
                like_count: 0,
                comment_count: 0,
                collect_count: 0,
                sb_version: 0,
            },
            reports: Vec::new(),
            judge_records: Value::Null,
            is_deal: false,
        };
        let mut suggestion = Suggestion {
            action: 1,
            violation_type: "无违规".to_string(),
            remark: "正常交流".to_string(),
            user_message: String::new(),
            reporter_message: String::new(),
            reason: String::new(),
            confidence: 0.4,
        };
        apply_link_policy(&case, &mut suggestion);
        assert_eq!(suggestion.action, 3);
        assert_eq!(suggestion.violation_type, "广告引流");
        assert_eq!(suggestion.user_message, USER_DELETE_MESSAGE);
        assert!(suggestion.confidence >= 0.95);
        assert!(suggestion.reason.contains("spam.example.net"));
    }

    #[test]
    fn action_accepts_numbers_and_labels() {
        assert_eq!(action_of(&json!(2)), Some(2));
        assert_eq!(action_of(&json!("3")), Some(3));
        assert_eq!(action_of(&json!("删除")), Some(3));
        assert_eq!(action_of(&json!("替换处理")), Some(2));
        assert_eq!(action_of(&json!("2（替换）")), Some(2));
        assert_eq!(action_of(&json!("正常显示")), Some(1));
        assert_eq!(action_of(&json!("keep")), Some(1));
        assert_eq!(action_of(&json!("不知道")), None);
        assert_eq!(action_of(&json!(null)), None);
    }

    #[test]
    fn missing_fields_fall_back_to_official_copy() {
        // 模型漏字段时用官方文案补齐，而不是让整条判定作废。
        let suggestion = match parse_suggestion(r#"{"action":3,"violationType":"刷屏"}"#) {
            Ok(suggestion) => suggestion,
            Err(error) => panic!("漏字段应当兜底而不是报错：{error}"),
        };
        assert_eq!(suggestion.remark, "刷屏");
        assert_eq!(suggestion.user_message, USER_DELETE_MESSAGE);
        assert_eq!(suggestion.reporter_message, REPORTER_MESSAGE);

        // 模型有时把数字写成字符串，这里应当照常接受。
        let suggestion = match parse_suggestion(
            r#"{"action":"2","remark":"广告引流","userMessage":"a","reporterMessage":"b","confidence":"0.82"}"#,
        ) {
            Ok(suggestion) => suggestion,
            Err(error) => panic!("应当接受字符串形式的字段：{error}"),
        };
        assert_eq!(suggestion.action, 2);
        assert!((suggestion.confidence - 0.82).abs() < f64::EPSILON);
    }

    #[test]
    fn tool_call_arguments_become_suggestion() {
        let payload = json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "type": "function",
                        "function": {
                            "name": "submit_review",
                            "arguments": "{\"action\":\"3\",\"violationType\":\"广告引流\",\"remark\":\" 加群广告。 \",\"reason\":\"评论含加群联系方式\",\"confidence\":\"0.91\"}"
                        }
                    }]
                }
            }]
        });
        let suggestion = match parse_tool_reply(&payload) {
            Some(suggestion) => suggestion,
            None => panic!("应当从工具参数解析出审核结论"),
        };
        assert_eq!(suggestion.action, 3);
        assert_eq!(suggestion.violation_type, "广告引流");
        assert_eq!(suggestion.remark, "加群广告");
        assert_eq!(suggestion.user_message, USER_DELETE_MESSAGE);
        assert_eq!(suggestion.reporter_message, REPORTER_MESSAGE);
        assert!((suggestion.confidence - 0.91).abs() < f64::EPSILON);
    }

    #[test]
    fn tool_mode_accepts_plain_json_content() {
        // 部分模型忽略 tools，直接在 content 里回 JSON，也要照常识别。
        let payload = json!({
            "choices": [{
                "message": {
                    "content": "```json\n{\"action\":2,\"violationType\":\"刷屏\",\"remark\":\"刷屏\",\"confidence\":0.8}\n```"
                }
            }]
        });
        let suggestion = parse_tool_reply(&payload).expect("content 里的 JSON 也应接受");
        assert_eq!(suggestion.action, 2);
        assert_eq!(suggestion.user_message, USER_REPLACE_MESSAGE);
    }

    #[test]
    fn normal_result_uses_fixed_copy_and_short_remark() {
        let suggestion = match parse_suggestion(
            r#"{"action":1,"violationType":"无违规","remark":"这是一段明显超过二十个字的受理备注用来测试截断逻辑是否生效"}"#,
        ) {
            Ok(suggestion) => suggestion,
            Err(error) => panic!("应当能解析正常显示的判定：{error}"),
        };
        assert_eq!(suggestion.user_message, USER_NORMAL_MESSAGE);
        assert_eq!(suggestion.reporter_message, REPORTER_NORMAL_MESSAGE);
        assert_eq!(suggestion.remark.chars().count(), REMARK_LIMIT);

        let empty = match parse_suggestion(r#"{"action":1,"violationType":"无违规","remark":""}"#) {
            Ok(suggestion) => suggestion,
            Err(error) => panic!("空备注应当自动补齐：{error}"),
        };
        assert_eq!(empty.remark, "未发现违规");
    }

    #[test]
    fn tool_schema_carries_submission_contract() {
        let tool = submit_review_tool();
        assert_eq!(tool["function"]["name"], "submit_review");
        let required = tool["function"]["parameters"]["required"]
            .as_array()
            .expect("工具参数应当声明必填字段");
        for field in ["action", "violationType", "remark", "reason", "confidence"] {
            assert!(required.iter().any(|item| item == field), "缺少必填字段 {field}");
        }
        // 站内信已由代码固定，提示词里不再让模型填写。
        let prompt = system_prompt();
        assert!(!prompt.contains("userMessage"));
        assert!(!prompt.contains("reporterMessage"));
        assert!(prompt.contains("submit_review"));
    }
}

mod ai;
mod auto;
mod case_times;
mod community;
mod error;
mod records;
mod satisfaction;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    env, io::Write,
    net::TcpStream,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tower_http::{
    cors::{Any, CorsLayer},
    services::{ServeDir, ServeFile},
};

use crate::{
    ai::AiConfig,
    community::{CaseDetail, CasePage, CaseSummary, Community, TOKEN_HEADER},
    error::ApiError,
};

/// 版本号：设置面板与页面底部展示。
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone)]
struct AppState {
    community: Community,
    http: reqwest::Client,
    ai: Option<AiConfig>,
    auto: auto::AutoRuntime,
    records: records::RecordsStore,
    /// 前端静态目录，用于 /api/version 探测页面是否更新。
    static_dir: PathBuf,
    /// 案件分类缓存：正常/隐藏分页需要扫描整份列表，短时间内复用。
    /// 多槽按 token 各留一份，命中只加一次引用计数、不再整表 clone。
    case_cache: Arc<Mutex<Vec<CaseCache>>>,
    /// 作品审核列表的独立分类缓存（与评论案件不互通）。
    composition_cache: Arc<Mutex<Vec<CaseCache>>>,
    /// 缓存过期后「后台重算」闸门：同一时刻只允许一次整表扫描。
    case_refreshing: Arc<AtomicBool>,
    composition_refreshing: Arc<AtomicBool>,
    /// 详情里的最早举报时间：列表接口经常不给或给错，分类前统一核对。
    case_times: case_times::CaseTimes,
    /// 多举报案件的认领信息（只有详情里有）：按案件号缓存最近 5 条。
    claim_cache: case_times::ClaimCache,
}

/// 分类后的案件：隐藏 = 已认领或超过一周。
#[derive(Clone)]
struct ClassifiedCase {
    summary: CaseSummary,
    hidden: bool,
    stale: bool,
}

struct CaseCache {
    token_hash: String,
    at: Instant,
    cases: Arc<[ClassifiedCase]>,
    /// 列表页数过多时只统计前面这些页，界面会提示。
    truncated: bool,
}

/// 一套分类缓存（评论列表 / 作品列表各一套）。
#[derive(Clone)]
struct CaseSlots {
    slots: Arc<Mutex<Vec<CaseCache>>>,
    refreshing: Arc<AtomicBool>,
    /// true = 扫描作品待审列表。
    composition: bool,
}

/// 缓存槽上限：多把 token 轮换时按写入顺序淘汰最旧的（写入即最新）。
const CASE_CACHE_SLOTS: usize = 4;

impl AppState {
    fn comment_slots(&self) -> CaseSlots {
        CaseSlots {
            slots: self.case_cache.clone(),
            refreshing: self.case_refreshing.clone(),
            composition: false,
        }
    }

    fn composition_slots(&self) -> CaseSlots {
        CaseSlots {
            slots: self.composition_cache.clone(),
            refreshing: self.composition_refreshing.clone(),
            composition: true,
        }
    }
}

/// 应用数据目录：默认系统盘（Windows 为 %LOCALAPPDATA%\QingFeng），
/// 这样更新/重装 exe 时 auto.json、待同步记录、日志都不会跟着目录一起丢。
/// 可用 QF_DATA_DIR 覆盖（容器部署指向挂载卷）。
pub(crate) fn data_dir() -> PathBuf {
    let dir = env::var("QF_DATA_DIR")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if cfg!(windows) {
                env::var("LOCALAPPDATA")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| env::temp_dir())
                    .join("QingFeng")
            } else {
                env::var("XDG_DATA_HOME")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| env::temp_dir())
                    .join("qingfeng")
            }
        });
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Windowed builds have no console, so important events also go to qingfeng.log.
pub(crate) fn log(message: impl AsRef<str>) {
    let text = message.as_ref();
    println!("{text}");
    let path = data_dir().join("qingfeng.log");
    // 旧版本日志写在 exe 同目录：首次启动复制过来（不删旧文件）。
    if !path.exists() {
        if let Some(legacy) = env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|parent| parent.join("qingfeng.log")))
        {
            if legacy.exists() {
                let _ = std::fs::copy(&legacy, &path);
            }
        }
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{text}");
    }
}

fn main() {
    dotenvy::dotenv().ok();
    let exe_dir = env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(PathBuf::from))
        .unwrap_or_default();
    if !exe_dir.as_os_str().is_empty() {
        dotenvy::from_path(exe_dir.join(".env")).ok();
    }

    let host = env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8080);
    let static_dir = resolve_static_dir(&exe_dir);
    let has_web = static_dir.join("index.html").exists();

    let runtime = tokio::runtime::Runtime::new().expect("无法初始化运行时");
    let server_host = host.clone();
    runtime.spawn(async move { serve(server_host, port, static_dir).await });

    let ready = wait_for_port(port, Duration::from_secs(8));
    if !ready {
        log(format!("服务未在预期时间内就绪（端口 {port}）"));
    }

    if has_web {
        log(format!(
            "无头服务模式：用浏览器访问 http://{host}:{port}（界面已托管，不需要单独的网站目录）"
        ));
    } else {
        log(format!(
            "无头服务模式：未找到前端静态目录，仅提供 /api 接口（STATIC_DIR 指向打包出来的 out/ 目录即可托管页面）"
        ));
    }
    thread::park();
}

fn resolve_static_dir(exe_dir: &PathBuf) -> PathBuf {
    match env::var("STATIC_DIR") {
        Ok(value) => PathBuf::from(value),
        // Packaged layout: <exe dir>/web next to the binary.
        Err(_) => {
            let beside = exe_dir.join("web");
            if beside.join("index.html").exists() {
                beside
            } else {
                PathBuf::from("../out")
            }
        }
    }
}

fn wait_for_port(port: u16, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        thread::sleep(Duration::from_millis(150));
    }
    false
}

async fn serve(host: String, port: u16, static_dir: PathBuf) {
    let exe_dir = env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(PathBuf::from))
        .unwrap_or_default();
    // 社区接口很快，20 秒足够；AI 请求用自带的 25 秒超时覆盖。
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("HTTP 客户端初始化失败");
    let community_base =
        env::var("COMMUNITY_API_BASE").unwrap_or_else(|_| "https://community-api.xiaomawang.com/api/v1".to_string());
    let ai = AiConfig::from_env();
    if ai.is_none() {
        log("AI 服务未配置：AI 辅助审核不可用，人工审核不受影响");
    }
    let auto = auto::AutoRuntime::new(&exe_dir);
    let community = Community::new(http.clone(), community_base);
    let records = records::RecordsStore::new(&exe_dir);
    // 7×24 后台自动审核：等端口绑定成功后再启动（见下方），
    // 否则重复启动的第二个进程也会跑一套循环，两个进程抢同一批案件。
    let auto_for_task = auto.clone();
    let community_for_task = community.clone();
    let http_for_task = http.clone();
    let ai_for_task = ai.clone();
    // 多举报案件的认领信息缓存：HTTP 端与后台自动审核共用同一份。
    let claim_cache = case_times::ClaimCache::new();
    let claim_cache_for_task = claim_cache.clone();
    // 满意度每日刷新：与自动审核分开跑，互不影响。
    let records_for_satisfaction = records.clone();
    let community_for_satisfaction = community.clone();
    let auto_for_satisfaction = auto.clone();
    let state = AppState {
        community,
        http,
        ai,
        auto,
        records,
        static_dir: static_dir.clone(),
        case_cache: Arc::new(Mutex::new(Vec::new())),
        composition_cache: Arc::new(Mutex::new(Vec::new())),
        case_refreshing: Arc::new(AtomicBool::new(false)),
        composition_refreshing: Arc::new(AtomicBool::new(false)),
        case_times: case_times::CaseTimes::load(&data_dir()),
        claim_cache: claim_cache.clone(),
    };

    let api = Router::new()
        .route("/health", get(health))
        .route("/version", get(app_version))
        .route("/session", get(session))
        .route("/ai-status", get(ai_status))
        .route(
            "/auto-config",
            get(auto_config_state).post(update_auto_config),
        )
        // 云端不再保存历史记录：/records 只读本地 JSON，历史由 /history/synced
        // 遍历社区「已审核」列表按审核员匹配得出。
        .route("/records", get(list_records))
        .route("/history/synced", get(synced_history))
        .route("/records/{case_id}", delete(delete_record))
        .route("/records/satisfaction/refresh", post(refresh_satisfaction))
        .route("/records/judge-id", post(update_judge_id))
        .route("/prefs", get(get_prefs).post(save_prefs))
        .route("/cases", get(list_cases))
        .route("/cases/{id}", get(case_detail))
        .route("/cases/{id}/ai-suggestion", post(ai_suggestion))
        .route("/cases/{id}/verify", post(verify_case))
        .route("/composition/cases", get(list_composition_cases))
        .route("/composition/cases/{id}", get(case_detail))
        .route("/composition/cases/{id}/verify", post(verify_composition))
        .route("/composition/history/synced", get(composition_synced_history))
        .route("/judged", post(judged_reports));

    // 退出信号处理器要用的那份 state（下面 with_state 会把原值移动进路由）。
    let shutdown_state = state.clone();
    let mut app = Router::new().nest("/api", api).with_state(state);

    if static_dir.join("index.html").exists() {
        let index = static_dir.join("index.html");
        app = app.fallback_service(ServeDir::new(&static_dir).fallback(ServeFile::new(index)));
        log(format!("托管前端静态文件：{}", static_dir.display()));
    }
    let app = app
        // 关键：index.html 绝不缓存。否则重新打包后浏览器仍加载旧 HTML，
        // 指向已被删除的旧 chunk，表现就是"改了代码但界面没变"。
        .layer(axum::middleware::from_fn(cache_headers))
        .layer(CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any));

    let listener = match tokio::net::TcpListener::bind((host.as_str(), port)).await {
        Ok(listener) => listener,
        Err(error) => {
            log(format!("端口 {port} 监听失败：{error}；请换一个 PORT，或先停掉已在运行的实例"));
            std::process::exit(1);
        }
    };
    // 只有抢到端口的实例才会走到这里，避免两个进程抢同一批案件。
    auto::spawn(
        auto_for_task,
        community_for_task,
        http_for_task,
        ai_for_task,
        claim_cache_for_task,
    );
    // 记录只要还在本地文件里就每天刷新一次满意度，不依赖浏览器是否打开。
    satisfaction::spawn(
        records_for_satisfaction,
        community_for_satisfaction,
        auto_for_satisfaction,
    );
    log(format!(
        "清风审核服务已启动：http://{host}:{port}（24 小时自动审核循环已就绪）"
    ));
    if let Err(error) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_state))
        .await
    {
        log(format!("服务异常退出：{error}"));
    }
}

/// 等退出信号，收到就落盘再退出。
/// 主线程只是 `thread::park()`，不处理 SIGTERM 的话 `docker stop` / 宝塔重启只能等
/// 超时被 SIGKILL，`case_report_times.json` 里刚核对好的举报时间就会丢。
async fn shutdown_signal(state: AppState) {
    wait_for_stop_signal().await;
    log("收到退出信号，正在保存举报时间缓存…");
    state.case_times.save();
    log("已保存，服务退出");
    // 主线程还 park 在那里，只能显式结束进程。
    std::process::exit(0);
}

#[cfg(unix)]
async fn wait_for_stop_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let Ok(mut terminate) = signal(SignalKind::terminate()) else {
        let _ = tokio::signal::ctrl_c().await;
        return;
    };
    tokio::select! {
        _ = terminate.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_stop_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn health(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "aiEnabled": state.ai.is_some(),
        "recordsDb": state.records.configured(),
    }))
}

/// 前端页面版本：用 index.html 的修改时间当指纹。
/// 前端轮询这个值，一旦变化就自动刷新，彻底解决"重新打包后界面还是旧的"。
async fn app_version(State(state): State<AppState>) -> Json<Value> {
    let build = std::fs::metadata(state.static_dir.join("index.html"))
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_millis().to_string())
        .unwrap_or_default();
    Json(json!({ "build": build, "version": APP_VERSION }))
}

/// 静态资源缓存策略：带哈希的 chunk 永久缓存，其余（尤其 index.html）不缓存。
async fn cache_headers(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::{header, HeaderValue};

    let immutable = request.uri().path().starts_with("/_next/static/");
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if immutable {
            "public, max-age=31536000, immutable"
        } else {
            "no-store, must-revalidate"
        }),
    );
    response
}

// ---- 审核记录（服务端本地 JSON，按社区 token 归属） ----

#[derive(Deserialize)]
struct RecordsQuery {
    limit: Option<i64>,
    #[serde(rename = "pageSize")]
    page_size: Option<i64>,
}

/// 同一账号 / 同一 token 的审核记录：任何设备登录后看到的都是同一份。
async fn list_records(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<RecordsQuery>,
) -> Result<Json<Value>, ApiError> {
    let ctx = session_ctx(&headers)?;
    let limit = query
        .limit
        .or(query.page_size)
        .unwrap_or(100)
        .clamp(1, 500);
    let (records, online) = state
        .records
        .list_for(None, &ctx.community_token, limit)
        .await;
    Ok(Json(json!({
        "records": records,
        "online": online,
        "configured": state.records.configured(),
        // 开源版没有账号体系，保留字段形状：前端据此显示「仅本机 token 可见」。
        "account": Value::Null,
        "tokenMasked": ctx.token_masked,
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistorySyncQuery {
    limit: Option<usize>,
    /// 最多扫描多少页已审核列表；每页固定 40 条。
    pages: Option<u32>,
    /// 设置里手动填写的审核员社区 ID；填了就优先使用。
    reviewer_id: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncedHistoryEntry {
    case_id: i64,
    nickname: String,
    content: String,
    last_report_time: i64,
}

/// 已审核列表每页条数 / 最大扫描页数 / 返回条数上限。
const HISTORY_SYNC_PAGE_SIZE: u32 = 40;
const HISTORY_SYNC_PAGES: u32 = 40;
const HISTORY_SYNC_LIMIT: usize = 200;
const HISTORY_SYNC_BUDGET: Duration = Duration::from_secs(30);

/// 遍历已审核案件列表，按 checkerId == 当前审核员社区 ID 生成历史记录。
/// 云端不再保存历史，这个接口是新的历史来源。
async fn synced_history(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HistorySyncQuery>,
) -> Result<Json<Value>, ApiError> {
    let token = session_ctx(&headers)?.token()?;
    let limit = query.limit.unwrap_or(HISTORY_SYNC_LIMIT).clamp(1, 500);
    let max_pages = query.pages.unwrap_or(HISTORY_SYNC_PAGES).clamp(1, 80);
    let started = Instant::now();

    // 设置里手动填写的审核员 ID 优先；没有再用 token / 最近判决案件反查。
    let mut reviewer_id = query.reviewer_id.filter(|id| *id > 0);
    if reviewer_id.is_none() {
        reviewer_id = records::token_user_id(&token);
    }
    if reviewer_id.is_none() {
        log(format!(
            "[history] token 无用户 ID 字段，payload keys={:?}",
            records::token_payload_keys(&token)
        ));
        let hint = state
            .community
            .latest_judged_case_id(&token)
            .await
            .unwrap_or(0);
        if hint > 0 {
            for page in 1..=max_pages.min(15) {
                if started.elapsed() > Duration::from_secs(10) {
                    break;
                }
                let fetched = tokio::time::timeout(
                    Duration::from_secs(10),
                    state
                        .community
                        .audited_cases(&token, page, HISTORY_SYNC_PAGE_SIZE),
                )
                .await;
                let result = match fetched {
                    Ok(Ok(result)) => result,
                    Ok(Err(error)) => {
                        if page == 1 {
                            return Err(error);
                        }
                        break;
                    }
                    Err(_) => break,
                };
                if let Some(item) = result
                    .list
                    .iter()
                    .find(|item| item.id == hint && item.checker_id > 0)
                {
                    reviewer_id = Some(item.checker_id);
                    log(format!(
                        "[history] 用最近判决案件 #{hint} 识别审核员 ID={}",
                        item.checker_id
                    ));
                    break;
                }
                if result.list.len() < HISTORY_SYNC_PAGE_SIZE as usize {
                    break;
                }
            }
        }
    }
    let Some(reviewer_id) = reviewer_id else {
        return Err(ApiError::BadRequest(
            "无法识别审核员 ID（token 缺少用户信息，且没有可用于反查的判决记录）".to_string(),
        ));
    };

    let mut entries: Vec<SyncedHistoryEntry> = Vec::new();
    let mut seen = HashSet::new();
    let mut scanned_pages = 0u32;
    let mut truncated = false;

    for page in 1..=max_pages {
        if entries.len() >= limit || started.elapsed() > HISTORY_SYNC_BUDGET {
            truncated = true;
            break;
        }
        let fetched = tokio::time::timeout(
            Duration::from_secs(10),
            state
                .community
                .audited_cases(&token, page, HISTORY_SYNC_PAGE_SIZE),
        )
        .await;
        let result = match fetched {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                if page == 1 {
                    return Err(error);
                }
                log(format!("[history] 已审核列表第 {page} 页失败：{error}"));
                truncated = true;
                break;
            }
            Err(_) => {
                truncated = true;
                break;
            }
        };
        scanned_pages = page;
        let len = result.list.len();
        for item in result.list {
            if item.checker_id != reviewer_id || !seen.insert(item.id) {
                continue;
            }
            entries.push(SyncedHistoryEntry {
                case_id: item.id,
                nickname: item.nickname,
                content: item.content,
                last_report_time: item.last_report_time,
            });
            if entries.len() >= limit {
                break;
            }
        }
        if len < HISTORY_SYNC_PAGE_SIZE as usize {
            break;
        }
    }

    entries.sort_by(|a, b| b.last_report_time.cmp(&a.last_report_time));
    log(format!(
        "[history] 已审核列表同步：审核员 {reviewer_id} 扫描 {scanned_pages} 页，找到 {} 条{}",
        entries.len(),
        if truncated { "（有截断）" } else { "" }
    ));
    Ok(Json(json!({
        "records": entries,
        "scannedPages": scanned_pages,
        "truncated": truncated,
        "reviewerId": reviewer_id,
    })))
}

/// 删除一条历史审核记录（本地文件 + 待补传队列）。
async fn delete_record(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(case_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    let ctx = session_ctx(&headers)?;
    state
        .records
        .delete_for(None, &ctx.community_token, case_id)
        .await
        .map_err(ApiError::BadRequest)?;
    Ok(Json(json!({ "ok": true, "caseId": case_id })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SatisfactionItem {
    case_id: i64,
    #[serde(default)]
    judge_record_id: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SatisfactionRefreshBody {
    #[serde(default)]
    items: Vec<SatisfactionItem>,
}

/// 刷新满意度：按案件号在「我已判决」列表里找 judgeRecordId，再取赞同 / 反对数自己算。
/// 客户端只在记录满 24 小时、且 24 小时内没刷过时调用。
async fn refresh_satisfaction(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SatisfactionRefreshBody>,
) -> Result<Json<Value>, ApiError> {
    let ctx = session_ctx(&headers)?;
    let token = ctx.token()?;
    // 记录按 token 归属，这里用同一个键，读写才对得上。
    let key = ctx.key();
    let items: Vec<(i64, i64)> = body
        .items
        .iter()
        .filter(|item| item.case_id > 0)
        .take(20)
        .map(|item| (item.case_id, item.judge_record_id))
        .collect();
    let results = satisfaction::refresh_batch(
        &state.records,
        &state.community,
        &token,
        Some(&key),
        &items,
        satisfaction::DISCOVER_PAGES,
        "manual",
    )
    .await;
    Ok(Json(json!({
        "items": results.iter().map(|item| item.to_json()).collect::<Vec<_>>()
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JudgeIdBody {
    case_id: i64,
    judge_record_id: i64,
}

/// 提交后补上判决记录 ID，第二天才能取满意度。
async fn update_judge_id(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<JudgeIdBody>,
) -> Result<Json<Value>, ApiError> {
    let ctx = session_ctx(&headers)?;
    let key = ctx.key();
    state
        .records
        .set_judge_record_id_by_key(&key, body.case_id, body.judge_record_id)
        .await
        .map_err(ApiError::BadRequest)?;
    Ok(Json(json!({ "ok": true })))
}

/// 只暴露掩码后的密钥状态，供前端显示轮转情况；这里不会返回任何明文密钥。
async fn ai_status(State(state): State<AppState>) -> Json<Value> {
    Json(match &state.ai {
        Some(config) => config.status(),
        None => json!({ "enabled": false, "keys": [] }),
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JudgedQuery {
    page: Option<u32>,
    page_size: Option<u32>,
}

/// 我已判决的案件列表，用于历史记录。
async fn judged_reports(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<JudgedQuery>,
) -> Result<Json<Value>, ApiError> {
    let token = session_ctx(&headers)?.token()?;
    let page = body.page.unwrap_or(1).max(1);
    let page_size = body.page_size.unwrap_or(50).clamp(1, 50);
    Ok(Json(state.community.judged_reports(&token, page, page_size).await?))
}

// ---- 请求身份：社区 access-token ----

fn header_value(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .unwrap_or_default()
        .to_string()
}

/// 一次请求的身份：开源版没有账号体系，社区 access-token 就是唯一凭证。
#[derive(Clone)]
struct SessionCtx {
    community_token: String,
    token_masked: String,
}

impl SessionCtx {
    /// 需要社区 token 的接口用这个。
    fn token(&self) -> Result<String, ApiError> {
        if self.community_token.trim().is_empty() {
            Err(ApiError::MissingToken)
        } else {
            Ok(self.community_token.clone())
        }
    }

    /// 记录与偏好的归属键：token 的 SHA-256，本地文件里不存明文 token。
    fn key(&self) -> String {
        records::token_hash(&self.community_token)
    }
}

/// 身份解析：只读请求头里的 access-token，不查库，所以不需要记忆缓存。
fn session_ctx(headers: &HeaderMap) -> Result<SessionCtx, ApiError> {
    let token = header_value(headers, TOKEN_HEADER);
    if token.is_empty() {
        return Err(ApiError::AuthRequired);
    }
    Ok(SessionCtx {
        token_masked: records::mask_token(&token),
        community_token: token,
    })
}

async fn session(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    let ctx = session_ctx(&headers)?;
    let unread = match state.community.unread(&ctx.community_token).await {
        Ok(unread) => unread,
        Err(error) => {
            log(format!("读取待办失败：{error}"));
            json!({ "total": 0, "pending": 0, "claimed": 0 })
        }
    };
    Ok(Json(json!(
        {
            "unread": unread,
            "aiEnabled": state.ai.is_some(),
            "community": {
                "hasToken": !ctx.community_token.trim().is_empty(),
                "masked": ctx.token_masked,
            },
        }
    )))
}

/// 服务器端 7×24 自动审核：查询运行状态（token 只回掩码）。
async fn auto_config_state(State(state): State<AppState>) -> Json<Value> {
    Json(state.auto.status())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AutoConfigPatch {
    enabled: Option<bool>,
    /// 提供且非空时才更新 token；留空表示沿用已保存的。
    token: Option<String>,
    interval: Option<u64>,
    threshold: Option<f64>,
    prefer_delete: Option<bool>,
}

async fn update_auto_config(
    State(state): State<AppState>,
    Json(body): Json<AutoConfigPatch>,
) -> Result<Json<Value>, ApiError> {
    let enabled = state.auto.update(|config| {
        if let Some(enabled) = body.enabled {
            config.enabled = enabled;
        }
        if let Some(token) = &body.token {
            let token = token.trim();
            if !token.is_empty() {
                config.token = token.to_string();
            }
        }
        if let Some(interval) = body.interval {
            config.interval = interval;
        }
        if let Some(threshold) = body.threshold {
            config.threshold = threshold;
        }
        if let Some(prefer_delete) = body.prefer_delete {
            config.prefer_delete = prefer_delete;
        }
    });
    log(format!(
        "[server-auto] 配置更新：enabled={} interval={}s threshold={:.2} preferDelete={} token={}",
        enabled.enabled,
        enabled.interval,
        enabled.threshold,
        enabled.prefer_delete,
        if enabled.token.is_empty() { "未配置" } else { "已配置" }
    ));
    Ok(Json(state.auto.status()))
}

// ---- 偏好设置（按 token 归属，存在本地文件里） ----

/// 读取当前 token 的偏好设置。
async fn get_prefs(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let ctx = session_ctx(&headers)?;
    let key = ctx.key();
    let (data, updated_at) = state
        .records
        .get_prefs_by_key(&key)
        .await
        .map_err(ApiError::BadRequest)?;
    Ok(Json(json!({ "data": data, "updatedAt": updated_at })))
}

#[derive(Deserialize)]
struct PrefsBody {
    data: Value,
}

/// 保存当前 token 的偏好设置（整份覆盖）。
async fn save_prefs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PrefsBody>,
) -> Result<Json<Value>, ApiError> {
    let ctx = session_ctx(&headers)?;
    state
        .records
        .save_prefs_by_key(&ctx.key(), &ctx.token_masked, &body.data)
        .await
        .map_err(ApiError::BadRequest)?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageQuery {
    page: Option<u32>,
    page_size: Option<u32>,
    /// 隐藏案件自己的页码：两组各自分页，总页数互不影响。
    hidden_page: Option<u32>,
    /// 按案件号搜索（上游 get-comment-list 的 reportId 参数）。
    report_id: Option<i64>,
    /// 自动审核等内部调用传 true，直接转发上游分页，不做分类扫描。
    raw: Option<bool>,
}

/// 分类扫描的上游每页条数、最多页数与时间预算。
const CLASSIFY_PAGE_SIZE: u32 = 50;
const CLASSIFY_MAX_PAGES: u32 = 20;
const CLASSIFY_BUDGET: Duration = Duration::from_secs(8);
/// 作品审核列表：每页条数、最多扫描页数（作品总数可能上千）。
const COMPOSITION_PAGE_SIZE: u32 = 40;
const COMPOSITION_MAX_PAGES: u32 = 30;
/// 分类时用详情核对举报时间：并发数、单条超时和总预算。
/// 首次核对后写进 case_report_times.json，后续刷新不再重复请求。
const RESOLVE_CONCURRENCY: usize = 6;
const RESOLVE_BUDGET: Duration = Duration::from_secs(20);
/// 分类缓存有效期：翻页、搜索时不用重复扫全表。要比前端 60 秒轮询长，
/// 否则每次轮询都是冷缓存；过期后走「先回旧数据、后台重算」。
const CASE_CACHE_TTL: Duration = Duration::from_secs(75);
/// 同时在飞的列表页数：一页发一次，不再串行等上一页。
const PAGE_WINDOW: usize = 4;

/// 列表响应：正常案件与隐藏案件各自有总数、页码和列表。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CaseView {
    total: i64,
    page: u32,
    page_size: u32,
    list: Vec<CaseSummary>,
    normal_total: i64,
    hidden_total: i64,
    hidden_claimed_total: i64,
    hidden_stale_total: i64,
    hidden_page: u32,
    hidden_page_size: u32,
    hidden_list: Vec<CaseSummary>,
    /// 本次是否为案件号搜索结果（此时两组都不再分页）。
    search: bool,
    /// 列表页数超过扫描上限：统计只覆盖前若干页。
    truncated: bool,
}

async fn list_cases(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> Result<Json<CaseView>, ApiError> {
    let token = session_ctx(&headers)?.token()?;
    let page = query.page.unwrap_or(1).max(1);
    let page_size = query.page_size.unwrap_or(10).clamp(1, 50);
    let hidden_page = query.hidden_page.unwrap_or(1).max(1);

    if let Some(report_id) = query.report_id.filter(|id| *id > 0) {
        return Ok(Json(search_case(&state, &token, report_id, page_size).await?));
    }

    if query.raw.unwrap_or(false) {
        let mut result = state.community.cases(&token, page, page_size, None).await?;
        fill_report_times(&state, &token, &mut result).await;
        log_case_sample(&result);
        return Ok(Json(CaseView {
            total: result.total,
            page,
            page_size,
            list: result.list,
            normal_total: result.total,
            hidden_total: 0,
            hidden_claimed_total: 0,
            hidden_stale_total: 0,
            hidden_page: 1,
            hidden_page_size: page_size,
            hidden_list: Vec::new(),
            search: false,
            truncated: false,
        }));
    }

    let (cases, truncated) = classified_cases(&state, &token).await?;
    let view = build_case_view(&cases, truncated, page, page_size, hidden_page);
    Ok(Json(view))
}

/// 按案件号搜索：带上 reportId 请求上游，并只保留精确匹配的案件。
async fn search_case(
    state: &AppState,
    token: &str,
    report_id: i64,
    page_size: u32,
) -> Result<CaseView, ApiError> {
    let mut result = state
        .community
        .cases(token, 1, page_size.max(10), Some(report_id))
        .await?;
    result.list.retain(|item| item.id == report_id);
    // 搜索结果也按详情时间判断，避免列表时间不准导致超期案件显示为可审。
    let verified = resolve_report_times(state, token, &mut result.list).await;
    // 多次举报的案件，认领信息只在详情里。
    apply_claim_names(state, token, &mut result.list).await;
    let mut normal = Vec::new();
    let mut hidden = Vec::new();
    let mut hidden_claimed = 0i64;
    let mut hidden_stale = 0i64;
    for (index, item) in result.list.into_iter().enumerate() {
        let checked = verified.contains(&index);
        let known = checked && auto::parse_when(&item.created_at).is_some();
        let stale = checked && auto::is_stale_text(&item.created_at);
        let claimed = !auto::first_unclaimed(&item.checker_name);
        let is_hidden = claimed || stale || !known;
        if is_hidden {
            if claimed {
                hidden_claimed += 1;
            }
            if stale || !known {
                hidden_stale += 1;
            }
            hidden.push(item);
        } else {
            normal.push(item);
        }
    }
    let normal_total = normal.len() as i64;
    let hidden_total = hidden.len() as i64;
    Ok(CaseView {
        total: normal_total,
        page: 1,
        page_size,
        list: normal,
        normal_total,
        hidden_total,
        hidden_claimed_total: hidden_claimed,
        hidden_stale_total: hidden_stale,
        hidden_page: 1,
        hidden_page_size: page_size,
        hidden_list: hidden,
        search: true,
        truncated: false,
    })
}

/// 扫描整份待审核列表并按「正常 / 隐藏」分类；结果按 token 缓存。
async fn classified_cases(
    state: &AppState,
    token: &str,
) -> Result<(Arc<[ClassifiedCase]>, bool), ApiError> {
    cached_cases(state, token, &state.comment_slots()).await
}

/// 缓存命中到哪一步了。
enum CacheHit {
    /// TTL 内，直接用。
    Fresh(Arc<[ClassifiedCase]>, bool),
    /// 已过期，但先拿旧数据顶住，重算交给后台。
    Stale(Arc<[ClassifiedCase]>, bool),
    /// 这个 token 还没扫过。
    Miss,
}

fn read_cache(slots: &Arc<Mutex<Vec<CaseCache>>>, hash: &str) -> CacheHit {
    let Ok(guard) = slots.lock() else {
        return CacheHit::Miss;
    };
    // 后写入的排在后面，从后往前找就是找最新的一份。
    for cache in guard.iter().rev() {
        if cache.token_hash != hash {
            continue;
        }
        return if cache.at.elapsed() < CASE_CACHE_TTL {
            CacheHit::Fresh(cache.cases.clone(), cache.truncated)
        } else {
            CacheHit::Stale(cache.cases.clone(), cache.truncated)
        };
    }
    CacheHit::Miss
}

fn write_cache(
    slots: &Arc<Mutex<Vec<CaseCache>>>,
    hash: &str,
    cases: Arc<[ClassifiedCase]>,
    truncated: bool,
) {
    if let Ok(mut guard) = slots.lock() {
        guard.retain(|cache| cache.token_hash != hash);
        guard.push(CaseCache {
            token_hash: hash.to_string(),
            at: Instant::now(),
            cases,
            truncated,
        });
        while guard.len() > CASE_CACHE_SLOTS {
            guard.remove(0);
        }
    }
}

/// 统一取数：新鲜缓存直接返回；过期立刻返回旧数据并在后台重算，
/// 这样前端 60 秒轮询永远不会再撞上十几秒的整表扫描。
async fn cached_cases(
    state: &AppState,
    token: &str,
    slots: &CaseSlots,
) -> Result<(Arc<[ClassifiedCase]>, bool), ApiError> {
    let hash = records::token_hash(token);
    match read_cache(&slots.slots, &hash) {
        CacheHit::Fresh(cases, truncated) => Ok((cases, truncated)),
        CacheHit::Stale(cases, truncated) => {
            spawn_scan(state.clone(), token.to_string(), slots.clone());
            Ok((cases, truncated))
        }
        CacheHit::Miss => {
            let (cases, truncated) = scan_cases(state, token, slots.composition).await?;
            let shared: Arc<[ClassifiedCase]> = cases.into();
            write_cache(&slots.slots, &hash, shared.clone(), truncated);
            Ok((shared, truncated))
        }
    }
}

async fn scan_cases(
    state: &AppState,
    token: &str,
    composition: bool,
) -> Result<(Vec<ClassifiedCase>, bool), ApiError> {
    if composition {
        sweep_compositions(state, token).await
    } else {
        sweep_cases(state, token).await
    }
}

/// 后台重算：抢到闸门才扫，避免过期瞬间的并发轮询各起一次整表扫描。
fn spawn_scan(state: AppState, token: String, slots: CaseSlots) {
    if slots
        .refreshing
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    tokio::spawn(async move {
        let hash = records::token_hash(&token);
        let scanned = scan_cases(&state, &token, slots.composition).await;
        slots.refreshing.store(false, Ordering::Release);
        if let Ok((cases, truncated)) = scanned {
            let shared: Arc<[ClassifiedCase]> = cases.into();
            write_cache(&slots.slots, &hash, shared, truncated);
        }
    });
}

/// 一次扫描任务的产出：一页列表，或一条详情核对到的举报时间。
enum SweepOut {
    Page {
        page: u32,
        total: i64,
        list: Vec<CaseSummary>,
        error: Option<ApiError>,
    },
    Detail {
        id: i64,
        /// `None` = 详情没请求成功（下次刷新继续核对）；空串 = 详情里也没有时间。
        time: Option<String>,
        /// 最新一条举报的 checkerName：空串/横线表示这个案件还没被认领。
        claim: Option<String>,
    },
}

fn spawn_page(
    joins: &mut tokio::task::JoinSet<SweepOut>,
    community: &Community,
    token: &str,
    page: u32,
    composition: bool,
) {
    let community = community.clone();
    let token = token.to_string();
    joins.spawn(async move {
        let fetched = tokio::time::timeout(
            Duration::from_secs(10),
            async {
                if composition {
                    community
                        .composition_cases(&token, page, COMPOSITION_PAGE_SIZE, None)
                        .await
                } else {
                    community
                        .cases(&token, page, CLASSIFY_PAGE_SIZE, None)
                        .await
                }
            },
        )
        .await;
        let (result, timed_out) = match fetched {
            Ok(result) => (result, false),
            Err(_) => (Err(ApiError::Upstream("列表请求超时".to_string())), true),
        };
        match result {
            Ok(page_data) => SweepOut::Page {
                page,
                total: page_data.total,
                list: page_data.list,
                error: None,
            },
            Err(error) => {
                let error = if timed_out {
                    ApiError::Upstream(format!("第 {page} 页请求超时"))
                } else {
                    error
                };
                SweepOut::Page {
                    page,
                    total: 0,
                    list: Vec::new(),
                    error: Some(error),
                }
            }
        }
    });
}

fn spawn_detail(joins: &mut tokio::task::JoinSet<SweepOut>, community: &Community, token: &str, id: i64) {
    let community = community.clone();
    let token = token.to_string();
    joins.spawn(async move {
        let detail = tokio::time::timeout(Duration::from_secs(4), community.case_detail(&token, id)).await;
        SweepOut::Detail {
            id,
            time: match &detail {
                Ok(Ok(detail)) => Some(detail.earliest_report_time()),
                _ => None,
            },
            claim: match &detail {
                Ok(Ok(detail)) => detail.latest_checker_name(),
                _ => None,
            },
        }
    });
}

/// 一页一详情重叠扫描：拿到一页就立刻查时间缓存，未命中的案件当场派去查详情。
/// 旧写法要等整表扫完才开始分波次核对，1000 条未缓存约等于 167 个串行波。
async fn sweep_cases(state: &AppState, token: &str) -> Result<(Vec<ClassifiedCase>, bool), ApiError> {
    let started = Instant::now();
    let mut joins = tokio::task::JoinSet::new();
    spawn_page(&mut joins, &state.community, token, 1, false);

    let mut pages_live = 1usize;
    let mut pages_spawned = 1u32;
    let mut pages_expected = 1u32;
    let mut details_live = 0usize;
    let mut pages: HashMap<u32, Vec<CaseSummary>> = HashMap::new();
    let mut queue: VecDeque<i64> = VecDeque::new();
    let mut queued: HashSet<i64> = HashSet::new();
    // 详情核对到的时间按案件号暂存，最后按页序回填（并发回来的顺序不是展示顺序）。
    let mut times: HashMap<i64, String> = HashMap::new();
    // 详情里最新一条举报的 checkerName（认领信息），按案件号回填到列表项。
    let mut claims: HashMap<i64, String> = HashMap::new();
    let mut verified: HashSet<i64> = HashSet::new();
    let mut truncated = false;
    let mut filled = 0usize;
    let mut failed = 0usize;

    while let Some(joined) = joins.join_next().await {
        match joined {
            Ok(SweepOut::Page {
                page,
                total,
                list,
                error,
            }) => {
                pages_live = pages_live.saturating_sub(1);
                if let Some(error) = error {
                    if page == 1 {
                        return Err(error);
                    }
                    log(format!("分类扫描第 {page} 页失败，已按前面结果统计：{error}"));
                    truncated = true;
                } else {
                    if page == 1 {
                        // 第 1 页就带 total：真实页数当场算出来，不用一页页试。
                        let size = CLASSIFY_PAGE_SIZE as i64;
                        let need = (total + size - 1) / size;
                        pages_expected = need.clamp(1, CLASSIFY_MAX_PAGES as i64) as u32;
                        if need > CLASSIFY_MAX_PAGES as i64 {
                            truncated = true;
                        }
                    }
                    if list.len() < CLASSIFY_PAGE_SIZE as usize {
                        pages_expected = pages_expected.min(page);
                    }
                    let mut bucket = Vec::with_capacity(list.len());
                    for item in list {
                        let mut need_detail = false;
                        match state.case_times.get(item.id) {
                            Some(time) => {
                                if !time.trim().is_empty() {
                                    times.insert(item.id, time);
                                }
                                verified.insert(item.id);
                            }
                            None => need_detail = true,
                        }
                        // 列表接口没有认领信息：举报次数大于 1 时必须看详情里最新那条
                        // 举报的 checkerName，否则会把这个案件当成「已认领」藏起来。
                        if item.report_num > 1 && !state.claim_cache.contains(item.id) {
                            need_detail = true;
                        }
                        if need_detail && queued.insert(item.id) {
                            queue.push_back(item.id);
                        }
                        bucket.push(item);
                    }
                    pages.insert(page, bucket);
                }
            }
            Ok(SweepOut::Detail { id, time, claim }) => {
                details_live = details_live.saturating_sub(1);
                if let Some(claim) = claim {
                    state.claim_cache.set(id, &claim);
                    claims.insert(id, claim);
                }
                match time {
                    Some(time) => {
                        if time.trim().is_empty() {
                            state.case_times.set_unknown(id);
                            failed += 1;
                        } else {
                            state.case_times.set(id, &time);
                            times.insert(id, time);
                            filled += 1;
                        }
                        verified.insert(id);
                    }
                    None => failed += 1,
                }
            }
            // 任务本身不会 panic（上游错误都包在 ApiError 里返回），
            // 真出现就只能按「少了一页」处理：统计范围变小，界面会提示。
            Err(_) => {
                pages_live = pages_live.saturating_sub(1);
                truncated = true;
            }
        }

        while pages_live < PAGE_WINDOW
            && pages_spawned < pages_expected
            && started.elapsed() <= CLASSIFY_BUDGET
        {
            pages_spawned += 1;
            spawn_page(&mut joins, &state.community, token, pages_spawned, false);
            pages_live += 1;
        }
        while details_live < RESOLVE_CONCURRENCY && started.elapsed() <= RESOLVE_BUDGET {
            let Some(id) = queue.pop_front() else { break };
            spawn_detail(&mut joins, &state.community, token, id);
            details_live += 1;
        }
    }

    if pages_spawned < pages_expected {
        truncated = true;
    }

    let mut keys: Vec<u32> = pages.keys().copied().collect();
    keys.sort_unstable();
    let mut cases = Vec::new();
    // 同一案件可能因为多条举报在列表里出现多行：只保留第一行，
    // 认领判定也以第一行（第一个出现的 checkerName）为准。
    let mut seen_cases: HashSet<i64> = HashSet::new();
    for key in keys {
        for mut item in pages.remove(&key).unwrap_or_default() {
            if !seen_cases.insert(item.id) {
                continue;
            }
            if let Some(time) = times.get(&item.id) {
                item.created_at = time.clone();
            }
            if let Some(claim) = claims.get(&item.id) {
                item.checker_name = claim.clone();
            }
            // 列表接口的 createdAt 经常为空甚至不对，只有详情里的最早举报时间可靠。
            // 超期、已认领、时间无法确认的案件都不进可审列表。
            let checked = verified.contains(&item.id);
            let claimed = !auto::first_unclaimed(&item.checker_name);
            let known = checked && auto::parse_when(&item.created_at).is_some();
            let stale = checked && auto::is_stale_text(&item.created_at);
            let hidden = claimed || stale || !known;
            cases.push(ClassifiedCase {
                summary: item,
                hidden,
                stale: stale || !known,
            });
        }
    }

    state.case_times.save_if_dirty();
    if filled > 0 || failed > 0 {
        log(format!(
            "核对举报时间 {filled} 条（无时间/失败 {failed}）；未完成的会在下次刷新继续"
        ));
    }
    Ok((cases, truncated))
}

/// 详情核对举报时间：缓存优先，未缓存的并发请求详情；总量受限，避免一次请求拖太久。
async fn resolve_report_times(
    state: &AppState,
    token: &str,
    items: &mut [CaseSummary],
) -> HashSet<usize> {
    let mut verified = HashSet::new();
    let mut pending: Vec<usize> = Vec::new();
    for (index, item) in items.iter_mut().enumerate() {
        match state.case_times.get(item.id) {
            Some(time) => {
                if !time.trim().is_empty() {
                    item.created_at = time;
                }
                verified.insert(index);
            }
            None => pending.push(index),
        }
    }
    if pending.is_empty() {
        return verified;
    }

    let started = Instant::now();
    let mut filled = 0usize;
    let mut failed = 0usize;
    for chunk in pending.chunks(RESOLVE_CONCURRENCY) {
        if started.elapsed() >= RESOLVE_BUDGET {
            break;
        }
        let mut joins = tokio::task::JoinSet::new();
        for index in chunk {
            let index = *index;
            let id = items[index].id;
            let community = state.community.clone();
            let token = token.to_string();
            joins.spawn(async move {
                let result = tokio::time::timeout(
                    Duration::from_secs(4),
                    community.case_detail(&token, id),
                )
                .await;
                (index, id, result)
            });
        }
        while let Some(joined) = joins.join_next().await {
            match joined {
                Ok((index, id, Ok(Ok(detail)))) => {
                    let time = detail.earliest_report_time();
                    if time.trim().is_empty() {
                        state.case_times.set_unknown(id);
                        failed += 1;
                    } else {
                        items[index].created_at = time.clone();
                        state.case_times.set(id, &time);
                        filled += 1;
                    }
                    verified.insert(index);
                }
                _ => failed += 1,
            }
        }
    }
    state.case_times.save_if_dirty();
    if filled > 0 || failed > 0 {
        log(format!(
            "核对举报时间 {filled} 条（无时间/失败 {failed}）；未完成的会在下次刷新继续"
        ));
    }
    verified
}

fn clear_case_cache(state: &AppState) {
    if let Ok(mut guard) = state.case_cache.lock() {
        guard.clear();
    }
}

fn build_case_view(
    cases: &[ClassifiedCase],
    truncated: bool,
    page: u32,
    page_size: u32,
    hidden_page: u32,
) -> CaseView {
    let normal: Vec<&ClassifiedCase> = cases.iter().filter(|item| !item.hidden).collect();
    let hidden: Vec<&ClassifiedCase> = cases.iter().filter(|item| item.hidden).collect();
    let normal_total = normal.len() as i64;
    let hidden_total = hidden.len() as i64;
    let hidden_claimed_total = hidden
        .iter()
        .filter(|item| !auto::first_unclaimed(&item.summary.checker_name))
        .count() as i64;
    let hidden_stale_total = hidden.iter().filter(|item| item.stale).count() as i64;

    let normal_page = CasePage {
        total: normal_total,
        page,
        page_size,
        list: slice_summaries(&normal, page, page_size),
    };
    let hidden_page_data = CasePage {
        total: hidden_total,
        page: hidden_page,
        page_size,
        list: slice_summaries(&hidden, hidden_page, page_size),
    };

    if page == 1 {
        log_case_sample(&normal_page);
    }

    CaseView {
        total: normal_total,
        page,
        page_size,
        list: normal_page.list,
        normal_total,
        hidden_total,
        hidden_claimed_total,
        hidden_stale_total,
        hidden_page,
        hidden_page_size: page_size,
        hidden_list: hidden_page_data.list,
        search: false,
        truncated,
    }
}

fn slice_summaries(items: &[&ClassifiedCase], page: u32, page_size: u32) -> Vec<CaseSummary> {
    let start = (page.saturating_sub(1) as usize).saturating_mul(page_size as usize);
    items
        .iter()
        .skip(start)
        .take(page_size as usize)
        .map(|item| item.summary.clone())
        .collect()
}

/// 作品待审列表分类；结果按 token 缓存，与评论列表同一套过期/后台重算逻辑。
async fn classified_compositions(
    state: &AppState,
    token: &str,
) -> Result<(Arc<[ClassifiedCase]>, bool), ApiError> {
    cached_cases(state, token, &state.composition_slots()).await
}

/// 作品列表只有分页并行：没有详情核对，超期直接按 lastReportTime 的 Unix 秒判断。
/// 列表接口不返回认领信息，只有 get-info 的 list 里有（按举报时间倒序，最新在前）。
/// 举报次数大于 1 的案件必须补查一次详情，把最新那条举报的 checkerName 覆盖回来，
/// 否则整案会被当成「已认领」藏起来。结果进 ClaimCache（只保留最近 5 个）。
async fn apply_claim_names(state: &AppState, token: &str, items: &mut [CaseSummary]) {
    let mut fetched = 0usize;
    for item in items.iter_mut() {
        if item.report_num <= 1 {
            continue;
        }
        if let Some(cached) = state.claim_cache.get(item.id) {
            item.checker_name = cached;
            continue;
        }
        if fetched >= 5 {
            break;
        }
        fetched += 1;
        let detail = tokio::time::timeout(
            Duration::from_secs(4),
            state.community.case_detail(token, item.id),
        )
        .await
        .ok()
        .and_then(|result| result.ok());
        if let Some(detail) = detail {
            if let Some(name) = detail.latest_checker_name() {
                state.claim_cache.set(item.id, &name);
                item.checker_name = name;
            }
        }
    }
}

async fn sweep_compositions(
    state: &AppState,
    token: &str,
) -> Result<(Vec<ClassifiedCase>, bool), ApiError> {
    let started = Instant::now();
    let mut joins = tokio::task::JoinSet::new();
    spawn_page(&mut joins, &state.community, token, 1, true);

    let mut pages_live = 1usize;
    let mut pages_spawned = 1u32;
    let mut pages_expected = 1u32;
    let mut pages: HashMap<u32, Vec<CaseSummary>> = HashMap::new();
    let mut truncated = false;

    while let Some(joined) = joins.join_next().await {
        match joined {
            Ok(SweepOut::Page {
                page,
                total,
                list,
                error,
            }) => {
                pages_live = pages_live.saturating_sub(1);
                if let Some(error) = error {
                    if page == 1 {
                        return Err(error);
                    }
                    log(format!("作品分类第 {page} 页失败，已按前面结果统计：{error}"));
                    truncated = true;
                } else {
                    if page == 1 {
                        let size = COMPOSITION_PAGE_SIZE as i64;
                        let need = (total + size - 1) / size;
                        pages_expected = need.clamp(1, COMPOSITION_MAX_PAGES as i64) as u32;
                        if need > COMPOSITION_MAX_PAGES as i64 {
                            truncated = true;
                        }
                    }
                    if list.len() < COMPOSITION_PAGE_SIZE as usize {
                        pages_expected = pages_expected.min(page);
                    }
                    pages.insert(page, list);
                }
            }
            Err(_) => {
                pages_live = pages_live.saturating_sub(1);
                truncated = true;
            }
            // 这里不会派详情任务，Detail 分支只为类型完整。
            Ok(_) => {}
        }

        while pages_live < PAGE_WINDOW
            && pages_spawned < pages_expected
            && started.elapsed() <= CLASSIFY_BUDGET
        {
            pages_spawned += 1;
            spawn_page(&mut joins, &state.community, token, pages_spawned, true);
            pages_live += 1;
        }
    }

    if pages_spawned < pages_expected {
        truncated = true;
    }

    let mut keys: Vec<u32> = pages.keys().copied().collect();
    keys.sort_unstable();
    let mut items: Vec<CaseSummary> = Vec::new();
    for key in keys {
        items.extend(pages.remove(&key).unwrap_or_default());
    }
    // 多次举报的案件，认领信息只在详情里：先补齐再分类。
    apply_claim_names(state, token, &mut items).await;
    let mut cases = Vec::new();
    for item in items {
        {
            // 已认领（checkerName 非空且不是横线）、超期、时间未知都进隐藏组。
            let claimed = !auto::first_unclaimed(&item.checker_name);
            let known = item.last_report_time > 0;
            let stale = auto::is_stale_unix(item.last_report_time);
            let hidden = claimed || stale || !known;
            cases.push(ClassifiedCase {
                summary: item,
                hidden,
                stale: stale || !known,
            });
        }
    }
    Ok((cases, truncated))
}

fn clear_composition_cache(state: &AppState) {
    if let Ok(mut guard) = state.composition_cache.lock() {
        guard.clear();
    }
}

async fn list_composition_cases(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> Result<Json<CaseView>, ApiError> {
    let token = session_ctx(&headers)?.token()?;
    let page = query.page.unwrap_or(1).max(1);
    let page_size = query.page_size.unwrap_or(10).clamp(1, 50);
    let hidden_page = query.hidden_page.unwrap_or(1).max(1);

    if let Some(report_id) = query.report_id.filter(|id| *id > 0) {
        return Ok(Json(
            composition_search(&state, &token, report_id, page_size).await?,
        ));
    }

    if query.raw.unwrap_or(false) {
        let result = state
            .community
            .composition_cases(&token, page, page_size, None)
            .await?;
        return Ok(Json(CaseView {
            total: result.total,
            page,
            page_size,
            list: result.list,
            normal_total: result.total,
            hidden_total: 0,
            hidden_claimed_total: 0,
            hidden_stale_total: 0,
            hidden_page: 1,
            hidden_page_size: page_size,
            hidden_list: Vec::new(),
            search: false,
            truncated: false,
        }));
    }

    let (cases, truncated) = classified_compositions(&state, &token).await?;
    Ok(Json(build_case_view(
        &cases,
        truncated,
        page,
        page_size,
        hidden_page,
    )))
}

async fn composition_search(
    state: &AppState,
    token: &str,
    report_id: i64,
    page_size: u32,
) -> Result<CaseView, ApiError> {
    let mut result = state
        .community
        .composition_cases(token, 1, page_size.max(10), Some(report_id))
        .await?;
    result.list.retain(|item| item.id == report_id);
    // 多次举报的作品，认领信息只在详情里。
    apply_claim_names(state, token, &mut result.list).await;
    let mut normal = Vec::new();
    let mut hidden = Vec::new();
    let mut hidden_claimed = 0i64;
    let mut hidden_stale = 0i64;
    for item in result.list {
        let claimed = !auto::first_unclaimed(&item.checker_name);
        let known = item.last_report_time > 0;
        let stale = auto::is_stale_unix(item.last_report_time);
        if claimed || stale || !known {
            if claimed {
                hidden_claimed += 1;
            }
            if stale || !known {
                hidden_stale += 1;
            }
            hidden.push(item);
        } else {
            normal.push(item);
        }
    }
    let normal_total = normal.len() as i64;
    let hidden_total = hidden.len() as i64;
    Ok(CaseView {
        total: normal_total,
        page: 1,
        page_size,
        list: normal,
        normal_total,
        hidden_total,
        hidden_claimed_total: hidden_claimed,
        hidden_stale_total: hidden_stale,
        hidden_page: 1,
        hidden_page_size: page_size,
        hidden_list: hidden,
        search: true,
        truncated: false,
    })
}

/// 给列表里缺少举报时间的条目补上真实时间（并发 4、单条 6 秒超时）。
/// 已经核对过的案件直接用 case_times 缓存，避免自动审核反复请求详情。
async fn fill_report_times(state: &AppState, token: &str, page: &mut CasePage) {
    let mut missing: Vec<usize> = Vec::new();
    for (index, item) in page.list.iter_mut().enumerate() {
        if item.id <= 0 {
            continue;
        }
        if let Some(time) = state.case_times.get(item.id) {
            if !time.trim().is_empty() {
                item.created_at = time;
            }
            continue;
        }
        if item.created_at.trim().is_empty() {
            missing.push(index);
        }
    }
    if missing.is_empty() {
        return;
    }
    let mut filled = 0usize;
    let mut failed = 0usize;
    for chunk in missing.chunks(4) {
        let mut joins = tokio::task::JoinSet::new();
        for index in chunk {
            let index = *index;
            let id = page.list[index].id;
            let community = state.community.clone();
            let token = token.to_string();
            joins.spawn(async move {
                let result = tokio::time::timeout(
                    Duration::from_secs(6),
                    community.case_detail(&token, id),
                )
                .await;
                (index, id, result)
            });
        }
        while let Some(joined) = joins.join_next().await {
            match joined {
                Ok((index, id, Ok(Ok(detail)))) => {
                    let time = detail.earliest_report_time();
                    if time.trim().is_empty() {
                        state.case_times.set_unknown(id);
                        failed += 1;
                    } else {
                        page.list[index].created_at = time.clone();
                        state.case_times.set(id, &time);
                        filled += 1;
                    }
                }
                _ => failed += 1,
            }
        }
    }
    state.case_times.save_if_dirty();
    log(format!(
        "补全举报时间 {filled} 条（无时间/失败 {failed}）；已核对的案件会复用缓存"
    ));
}

/// 第一次拿到列表时记录几条 checkerName，便于核对「未认领」的判定依据。
fn log_case_sample(page: &CasePage) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if LOGGED.swap(true, Ordering::SeqCst) {
        return;
    }
    let sample = page
        .list
        .iter()
        .take(8)
        .map(|item| {
            let name = item.checker_name.trim();
            let shown = if name.is_empty() { "空" } else { name };
            format!("{}[{}]", item.id, shown)
        })
        .collect::<Vec<_>>()
        .join(" ");
    log(format!("案件列表样例（checkerName）：{sample}"));
}

async fn case_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<CaseDetail>, ApiError> {
    let token = session_ctx(&headers)?.token()?;
    Ok(Json(state.community.case_detail(&token, id).await?))
}

async fn ai_suggestion(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<ai::Suggestion>, ApiError> {
    // 开源版没有角色体系：只要配了 AI 就能用建议。
    let ctx = session_ctx(&headers)?;
    let token = ctx.token()?;
    let config = state.ai.as_ref().ok_or(ApiError::AiUnavailable)?;
    let case = state.community.case_detail(&token, id).await?;
    // 超过一周或举报时间无法确认的案件不再分析：这类案件通常早已失效。
    if auto::is_stale(&case) {
        return Err(ApiError::BadRequest(
            "案件举报已超过一周或举报时间未知，系统不再分析".to_string(),
        ));
    }
    let suggestion = ai::suggest(&state.http, config, &case).await?;
    log(format!(
        "AI 建议 案件#{id}: action={} confidence={:.2} remark={}",
        suggestion.action, suggestion.confidence, suggestion.remark
    ));
    Ok(Json(suggestion))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VerifyRequest {
    action: u8,
    remark: String,
    /// 评论审核必填；作品审核不发送被举报人站内信，可省略。
    #[serde(default)]
    user_message: String,
    reporter_message: String,
}

async fn verify_case(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(body): Json<VerifyRequest>,
) -> Result<Json<Value>, ApiError> {
    let ctx = session_ctx(&headers)?;
    let token = ctx.token()?;
    if !(1..=3).contains(&body.action) {
        return Err(ApiError::BadRequest("请选择受理结果".to_string()));
    }
    let remark = body.remark.trim();
    let user_message = body.user_message.trim();
    let reporter_message = body.reporter_message.trim();
    if remark.is_empty() || user_message.is_empty() || reporter_message.is_empty() {
        return Err(ApiError::BadRequest("受理备注和两条站内信均为必填".to_string()));
    }

    // 先拉详情：既用于受理全部举报记录，也用于拦截已超期/时间未知的案件。
    let case = state.community.case_detail(&token, id).await?;
    if auto::is_stale(&case) {
        return Err(ApiError::BadRequest(
            "案件举报已超过一周或举报时间未知，系统不再提交审核".to_string(),
        ));
    }
    match state
        .community
        .verify_report(&token, &case, body.action, remark, user_message, reporter_message)
        .await
    {
        Ok(accepted) => {
            log(format!(
                "提交成功 案件#{}: action={} 举报记录={} 条",
                case.report_id, body.action, accepted
            ));
            // 分类缓存立即失效，刷新列表时案件马上进入「已隐藏」。
            clear_case_cache(&state);
            // 云端不再保存历史记录。历史同步后续改为遍历已审核案件列表，
            // 按 checkerName 匹配当前审核员后再生成；接口方式确认前先不写入。
            Ok(Json(json!({
                "ok": true,
                "reportId": case.report_id,
                "acceptedReports": accepted,
            })))
        }
        Err(error) => {
            log(format!("提交失败 案件#{id}: {error}"));
            Err(error)
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompositionHistoryQuery {
    reviewer_id: Option<i64>,
    limit: Option<usize>,
    pages: Option<u32>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncedCompositionEntry {
    case_id: i64,
    nickname: String,
    title: String,
    last_report_time: i64,
}

/// 提交作品审核：/report/verify-composition。作品不抓取满意度。
async fn verify_composition(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(body): Json<VerifyRequest>,
) -> Result<Json<Value>, ApiError> {
    let token = session_ctx(&headers)?.token()?;
    // 作品只有两种结论：2 = 下架，3 = 正常显示。
    if body.action != 2 && body.action != 3 {
        return Err(ApiError::BadRequest("请选择受理结果：正常显示或下架".to_string()));
    }
    let remark = body.remark.trim();
    let reporter_message = body.reporter_message.trim();
    let user_message = body.user_message.trim();
    if remark.is_empty() || reporter_message.is_empty() {
        return Err(ApiError::BadRequest(
            "受理备注和举报人站内信均为必填".to_string(),
        ));
    }
    if body.action == 2 && user_message.is_empty() {
        return Err(ApiError::BadRequest(
            "下架需要填写被举报人站内信".to_string(),
        ));
    }

    let case = state.community.case_detail(&token, id).await?;
    if case.target_type != "compose" {
        return Err(ApiError::BadRequest("不是作品举报案件".to_string()));
    }
    if auto::is_stale(&case) {
        return Err(ApiError::BadRequest(
            "作品举报已超过一周或举报时间未知，系统不再提交审核".to_string(),
        ));
    }
    // 认领状态交给上游判断：列表 checkerId 和详情 checkerId 偶有不一致，
    // 这里不再本地拦截，而是把真实的上游错误（如没有权限）返回给界面。

    match state
        .community
        .verify_composition(
            &token,
            &case,
            body.action,
            remark,
            user_message,
            reporter_message,
        )
        .await
    {
        Ok(accepted) => {
            log(format!(
                "作品提交成功 案件#{}: action={} 举报记录={} 条",
                case.report_id, body.action, accepted
            ));
            clear_composition_cache(&state);
            Ok(Json(json!({
                "ok": true,
                "reportId": case.report_id,
                "acceptedReports": accepted,
            })))
        }
        Err(error) => {
            log(format!("作品提交失败 案件#{id}: {error}"));
            Err(error)
        }
    }
}

/// 遍历已审核作品列表，按 checkerId == 手动填写的审核员 ID 生成作品历史。
async fn composition_synced_history(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CompositionHistoryQuery>,
) -> Result<Json<Value>, ApiError> {
    let token = session_ctx(&headers)?.token()?;
    let reviewer_id = query.reviewer_id.unwrap_or(0);
    if reviewer_id <= 0 {
        return Err(ApiError::BadRequest(
            "请先在设置里填写审核员 ID（社区 checkerId）".to_string(),
        ));
    }
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let max_pages = query.pages.unwrap_or(HISTORY_SYNC_PAGES).clamp(1, 80);
    let started = Instant::now();
    let mut entries: Vec<SyncedCompositionEntry> = Vec::new();
    let mut seen = HashSet::new();
    let mut scanned_pages = 0u32;
    let mut truncated = false;

    for page in 1..=max_pages {
        if entries.len() >= limit || started.elapsed() > HISTORY_SYNC_BUDGET {
            truncated = true;
            break;
        }
        let fetched = tokio::time::timeout(
            Duration::from_secs(10),
            state
                .community
                .audited_composition_cases(&token, page, HISTORY_SYNC_PAGE_SIZE),
        )
        .await;
        let result = match fetched {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                if page == 1 {
                    return Err(error);
                }
                log(format!("[composition] 已审核作品第 {page} 页失败：{error}"));
                truncated = true;
                break;
            }
            Err(_) => {
                truncated = true;
                break;
            }
        };
        scanned_pages = page;
        let len = result.list.len();
        for item in result.list {
            if item.checker_id != reviewer_id || !seen.insert(item.id) {
                continue;
            }
            entries.push(SyncedCompositionEntry {
                case_id: item.id,
                nickname: item.nickname,
                title: item.title,
                last_report_time: item.last_report_time,
            });
            if entries.len() >= limit {
                break;
            }
        }
        if len < HISTORY_SYNC_PAGE_SIZE as usize {
            break;
        }
    }

    entries.sort_by(|a, b| b.last_report_time.cmp(&a.last_report_time));
    log(format!(
        "[composition] 已审核作品同步：审核员 {reviewer_id} 扫描 {scanned_pages} 页，找到 {} 条{}",
        entries.len(),
        if truncated { "（有截断）" } else { "" }
    ));
    Ok(Json(json!({
        "records": entries,
        "scannedPages": scanned_pages,
        "truncated": truncated,
        "reviewerId": reviewer_id,
    })))
}
/// 开源版只有「贴 token」这一种身份，这里把它的边界固定下来。
#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with_token(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::HeaderName::from_static(TOKEN_HEADER),
            HeaderValue::from_str(token).expect("token 必须是合法头值"),
        );
        headers
    }

    #[test]
    fn missing_token_is_a_plain_auth_error() {
        // AuthRequired 会带 authExpired，前端据此回到贴 token 的入口。
        match session_ctx(&HeaderMap::new()) {
            Ok(_) => panic!("没有 access-token 头时不应该放行"),
            Err(error) => assert!(matches!(error, ApiError::AuthRequired)),
        }
        assert!(session_ctx(&headers_with_token("   ")).is_err());
    }

    #[test]
    fn token_is_trimmed_and_never_echoed_back_in_full() {
        let token = "  eyJhbGciOi.test.abcdef  ";
        let ctx = session_ctx(&headers_with_token(token))
            .ok()
            .expect("两端带空白的 token 也要能用");
        assert_eq!(ctx.community_token, token.trim());
        assert_eq!(ctx.key(), records::token_hash(token.trim()));
        assert!(!ctx.token_masked.contains("test"), "掩码不能露出中间内容");
    }

    #[test]
    fn cache_ttl_is_longer_than_the_frontend_poll() {
        // 前端每 60 秒轮询一次列表；TTL 短于它会让每次轮询都撞上整表重算。
        assert!(CASE_CACHE_TTL > Duration::from_secs(60));
        assert!(CASE_CACHE_SLOTS >= 2, "多槽缓存至少要能容纳轮换中的两把 token");
    }
}

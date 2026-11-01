use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

pub enum ApiError {
    MissingToken,
    SessionExpired,
    /// 请求没带社区 access-token（前端据此回到贴 token 的入口）。
    AuthRequired,
    BadRequest(String),
    Business { code: i64, message: String },
    Upstream(String),
    AiUnavailable,
    Ai(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::MissingToken => write!(formatter, "缺少 access-token"),
            ApiError::SessionExpired => write!(formatter, "登录已过期"),
            ApiError::AuthRequired => write!(formatter, "请先粘贴社区 access-token"),
            ApiError::BadRequest(message) => write!(formatter, "{message}"),
            ApiError::Business { code, message } => write!(formatter, "{message}（code={code}）"),
            ApiError::Upstream(message) => write!(formatter, "{message}"),
            ApiError::AiUnavailable => write!(formatter, "AI 服务未配置"),
            ApiError::Ai(message) => write!(formatter, "{message}"),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let auth_expired = matches!(self, ApiError::AuthRequired | ApiError::SessionExpired);
        let (status, code, message) = match self {
            ApiError::MissingToken => (StatusCode::UNAUTHORIZED, 10021, "请先输入 access-token".to_string()),
            ApiError::SessionExpired => (
                StatusCode::UNAUTHORIZED,
                10021,
                "登录已过期，请重新输入 token".to_string(),
            ),
            ApiError::AuthRequired => (
                StatusCode::UNAUTHORIZED,
                10022,
                "请先粘贴社区 access-token".to_string(),
            ),
            ApiError::BadRequest(message) => (StatusCode::BAD_REQUEST, 400, message),
            ApiError::Business { code, message } => (StatusCode::UNPROCESSABLE_ENTITY, code, message),
            ApiError::Upstream(message) => (StatusCode::BAD_GATEWAY, 502, message),
            ApiError::AiUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                503,
                "AI 服务未配置，请设置 AI_BASE_URL、AI_API_KEY 和 AI_MODEL".to_string(),
            ),
            ApiError::Ai(message) => (StatusCode::BAD_GATEWAY, 502, message),
        };
        let mut body = json!({ "code": code, "message": message });
        if auth_expired {
            body["authExpired"] = json!(true);
        }
        (status, Json(body)).into_response()
    }
}

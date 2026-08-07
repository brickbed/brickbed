use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found")]
    NotFound,

    #[error("unauthorized")]
    Unauthorized,

    #[error("forbidden")]
    Forbidden,

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("internal error: {0}")]
    Internal(String),

    /// An upstream embedding provider failed. The write is refused rather than
    /// persisted without its vector.
    #[error("embedding provider error: {0}")]
    Embedding(String),
    /// An extractor rejected the request before a handler saw it. Carries the
    /// extractor's own status so axum's 400-vs-422 distinction survives, while
    /// the body still comes back in our `{"error": ...}` shape.
    #[error("{1}")]
    Rejection(StatusCode, String),
}

impl From<crate::rules::RuleError> for AppError {
    fn from(error: crate::rules::RuleError) -> Self {
        match error {
            // A credential might help.
            crate::rules::RuleError::Unauthenticated => AppError::Unauthorized,
            // It would not.
            crate::rules::RuleError::Forbidden => AppError::Forbidden,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            AppError::NotFound => (StatusCode::NOT_FOUND, "not found"),
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            AppError::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.as_str()),
            AppError::Internal(msg) => {
                tracing::error!(error = %msg, "internal request failure");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error")
            }
            AppError::Embedding(msg) => (StatusCode::BAD_GATEWAY, msg.as_str()),
            AppError::Rejection(status, msg) => (*status, msg.as_str()),
        };

        let body = Json(json!({
            "error": message
        }));

        (status, body).into_response()
    }
}

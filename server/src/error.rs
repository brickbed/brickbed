//! Stable, public HTTP error contract.
//!
//! Error *messages* help a person repair a request. [`ErrorCode`] is the
//! machine contract: clients must branch on it rather than parsing a message.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use serde_json::{json, Map, Value};

tokio::task_local! {
    pub(crate) static REQUEST_ID: String;
}

/// Bounded request identifier supplied by the request-id middleware.
pub fn request_id() -> String {
    REQUEST_ID
        .try_with(Clone::clone)
        // Direct library callers do not have HTTP middleware. Never expose a
        // sentinel that a client could mistake for a real request identifier.
        .unwrap_or_else(|_| ulid::Ulid::new().to_string())
}

/// The complete registry of machine-readable error codes for error contract
/// v1. New codes are additive; clients must preserve unknown strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    ValidationFailed,
    SchemaInvalid,
    InvalidCursor,
    Conflict,
    IdempotencyConflict,
    Unauthorized,
    Forbidden,
    NotFound,
    LimitExceeded,
    RateLimited,
    Unavailable,
    EmbeddingProviderError,
    InternalError,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorBody {
    code: ErrorCode,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<Map<String, Value>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorEnvelope {
    error: ErrorBody,
    request_id: String,
}

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

    #[error("validation failed: {0}")]
    Validation(String),

    #[error("schema invalid: {0}")]
    Schema(String),

    #[error("invalid cursor: {0}")]
    InvalidCursor(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("idempotency conflict: {0}")]
    IdempotencyConflict(String),

    #[error("limit exceeded: {0}")]
    Limit(String),

    #[error("rate limited: {0}")]
    RateLimited(String),

    #[error("unavailable: {0}")]
    Unavailable(String),

    #[error("internal error: {0}")]
    Internal(String),

    /// An upstream embedding provider failed. Its message is deliberately
    /// retained only for logs: provider bodies can contain secrets or data.
    #[error("embedding provider error: {0}")]
    Embedding(String),
    /// An extractor rejected the request before a handler saw it. Carries the
    /// extractor's status while still using this module's JSON envelope.
    #[error("{1}")]
    Rejection(StatusCode, String),
}

impl From<crate::rules::RuleError> for AppError {
    fn from(error: crate::rules::RuleError) -> Self {
        match error {
            crate::rules::RuleError::Unauthenticated => AppError::Unauthorized,
            crate::rules::RuleError::Forbidden => AppError::Forbidden,
        }
    }
}

impl AppError {
    fn public_parts(&self) -> (StatusCode, ErrorCode, String, Option<Map<String, Value>>) {
        match self {
            Self::NotFound => (StatusCode::NOT_FOUND, ErrorCode::NotFound, "not found".into(), None),
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                ErrorCode::Unauthorized,
                "authentication is required or the credential is invalid".into(),
                None,
            ),
            Self::Forbidden => (
                StatusCode::FORBIDDEN,
                ErrorCode::Forbidden,
                "the credential is not allowed to perform this operation".into(),
                None,
            ),
            Self::BadRequest(message) => (
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidRequest,
                message.clone(),
                None,
            ),
            Self::Validation(message) => (
                StatusCode::BAD_REQUEST,
                ErrorCode::ValidationFailed,
                message.clone(),
                None,
            ),
            Self::Schema(message) => (
                StatusCode::BAD_REQUEST,
                ErrorCode::SchemaInvalid,
                message.clone(),
                None,
            ),
            Self::InvalidCursor(message) => (
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidCursor,
                message.clone(),
                None,
            ),
            Self::Conflict(message) => (
                StatusCode::CONFLICT,
                ErrorCode::Conflict,
                message.clone(),
                None,
            ),
            Self::IdempotencyConflict(message) => (
                StatusCode::CONFLICT,
                ErrorCode::IdempotencyConflict,
                message.clone(),
                None,
            ),
            Self::Limit(message) => (
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorCode::LimitExceeded,
                message.clone(),
                None,
            ),
            Self::RateLimited(message) => (
                StatusCode::TOO_MANY_REQUESTS,
                ErrorCode::RateLimited,
                message.clone(),
                None,
            ),
            Self::Unavailable(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::Unavailable,
                "the service is temporarily unavailable; retry the request".into(),
                None,
            ),
            Self::Embedding(_) => (
                StatusCode::BAD_GATEWAY,
                ErrorCode::EmbeddingProviderError,
                "the embedding provider request failed; retry the write or check provider configuration"
                    .into(),
                None,
            ),
            // SlateDB fences an older client once another writer claims the
            // database. That is temporary/unavailable from an HTTP client's
            // perspective, but the underlying storage text stays in logs.
            Self::Internal(message) if message.contains("detected newer DB client") => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::Unavailable,
                "the service is temporarily unavailable; retry the request".into(),
                None,
            ),
            Self::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::InternalError,
                "an internal error occurred; retry the request or contact support with the request ID"
                    .into(),
                None,
            ),
            Self::Rejection(status, message) => (
                *status,
                ErrorCode::InvalidRequest,
                message.clone(),
                None,
            ),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let request_id = request_id();
        // Only internal/server-facing messages are logged. They are never
        // inserted in a public envelope, including provider response bodies.
        match &self {
            Self::Internal(message) | Self::Unavailable(message) | Self::Embedding(message) => {
                tracing::error!(%request_id, error = %message, "request failed");
            }
            _ => {}
        }
        let (status, code, message, details) = self.public_parts();
        (
            status,
            Json(ErrorEnvelope {
                error: ErrorBody {
                    code,
                    message,
                    details,
                },
                request_id,
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_serializes_stable_codes() {
        assert_eq!(
            serde_json::to_value(ErrorCode::InvalidCursor).unwrap(),
            json!("invalid_cursor")
        );
        assert_eq!(
            serde_json::to_value(ErrorCode::EmbeddingProviderError).unwrap(),
            json!("embedding_provider_error")
        );
    }

    #[test]
    fn every_error_variant_has_a_stable_public_status_and_code() {
        let cases = [
            (AppError::BadRequest("bad".into()), StatusCode::BAD_REQUEST, ErrorCode::InvalidRequest),
            (AppError::Validation("bad".into()), StatusCode::BAD_REQUEST, ErrorCode::ValidationFailed),
            (AppError::Schema("bad".into()), StatusCode::BAD_REQUEST, ErrorCode::SchemaInvalid),
            (AppError::InvalidCursor("bad".into()), StatusCode::BAD_REQUEST, ErrorCode::InvalidCursor),
            (AppError::Conflict("bad".into()), StatusCode::CONFLICT, ErrorCode::Conflict),
            (AppError::IdempotencyConflict("bad".into()), StatusCode::CONFLICT, ErrorCode::IdempotencyConflict),
            (AppError::Limit("bad".into()), StatusCode::PAYLOAD_TOO_LARGE, ErrorCode::LimitExceeded),
            (AppError::RateLimited("bad".into()), StatusCode::TOO_MANY_REQUESTS, ErrorCode::RateLimited),
            (AppError::Unavailable("bad".into()), StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable),
            (AppError::Embedding("bad".into()), StatusCode::BAD_GATEWAY, ErrorCode::EmbeddingProviderError),
            (AppError::Internal("bad".into()), StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::InternalError),
            (AppError::NotFound, StatusCode::NOT_FOUND, ErrorCode::NotFound),
            (AppError::Unauthorized, StatusCode::UNAUTHORIZED, ErrorCode::Unauthorized),
            (AppError::Forbidden, StatusCode::FORBIDDEN, ErrorCode::Forbidden),
        ];
        for (error, status, code) in cases {
            let (actual_status, actual_code, _, details) = error.public_parts();
            assert_eq!(actual_status, status);
            assert_eq!(actual_code, code);
            assert!(details.is_none());
        }
    }

    #[test]
    fn internal_and_provider_messages_are_never_public() {
        for error in [
            AppError::Internal("/private/bucket/token=secret".into()),
            AppError::Embedding("provider body contains api_key=secret".into()),
            AppError::Internal("detected newer DB client at /private/bucket".into()),
        ] {
            let (_, _, message, details) = error.public_parts();
            assert!(!message.contains("secret"));
            assert!(!message.contains("/private"));
            assert!(details.is_none());
        }
    }

    #[test]
    fn request_id_is_generated_without_http_middleware() {
        let id = request_id();
        assert!(id.len() >= 8);
        assert!(id.bytes().all(|byte| byte.is_ascii_alphanumeric()));
    }
}

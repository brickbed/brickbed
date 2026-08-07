pub mod auth;
pub mod config;
pub mod db;
pub mod embed;
pub mod error;
pub mod extract;
pub mod fts;
pub mod handlers;
pub mod index;
pub mod jwt;
pub mod keybroker;
pub mod rules;
pub mod schema;
pub mod validate;
pub mod vector;

use axum::{
    extract::Request,
    http::{header::HeaderName, HeaderValue},
    middleware,
    response::Response,
    routing::{delete, get, patch, post, put},
    Router,
};
use std::sync::Arc;
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};

use handlers::AppState;

const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

fn valid_request_id(value: &str) -> bool {
    (8..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// Give every response a bounded correlation id. A caller-provided id is
/// accepted only when it is safe to reflect in a response header and log.
async fn request_id(request: Request, next: middleware::Next) -> Response {
    let id = request
        .headers()
        .get(&REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| valid_request_id(value))
        .map(str::to_owned)
        .unwrap_or_else(|| ulid::Ulid::new().to_string());

    crate::error::REQUEST_ID
        .scope(id.clone(), async move {
            let mut response = next.run(request).await;
            // IDs are ASCII by construction, so this cannot fail.
            response.headers_mut().insert(
                REQUEST_ID_HEADER,
                HeaderValue::from_str(&id).expect("validated request ID header"),
            );
            response
        })
        .await
}

async fn not_found() -> crate::error::AppError {
    crate::error::AppError::NotFound
}

async fn method_not_allowed() -> crate::error::AppError {
    crate::error::AppError::Rejection(
        axum::http::StatusCode::METHOD_NOT_ALLOWED,
        "method is not allowed for this route".to_string(),
    )
}

/// Path templates for every `/v1` route, used to build the router below so the
/// two cannot disagree on spelling.
pub const SCHEMA_PATH: &str = "/v1/:project/_schema";
pub const COLLECTION_PATH: &str = "/v1/:project/:collection";
pub const QUERY_PATH: &str = "/v1/:project/:collection/_query";
pub const SEARCH_PATH: &str = "/v1/:project/:collection/_search";
pub const DOCUMENT_PATH: &str = "/v1/:project/:collection/:id";

/// Every authenticated route, as `(method, path template)`.
///
/// Since [`auth::require_auth`] now lets anonymous requests reach handlers so
/// `"public"` rules can work, a handler that forgets its `rules::` guard is an
/// open endpoint. The route-coverage test walks this table anonymously and
/// asserts each route denies, and separately asserts the table still matches
/// the routes registered below — a new route cannot dodge coverage by being
/// left out of the list.
pub const V1_ROUTES: &[(&str, &str)] = &[
    ("PUT", SCHEMA_PATH),
    ("GET", SCHEMA_PATH),
    ("POST", COLLECTION_PATH),
    ("GET", COLLECTION_PATH),
    ("POST", QUERY_PATH),
    ("POST", SEARCH_PATH),
    ("GET", DOCUMENT_PATH),
    ("PUT", DOCUMENT_PATH),
    ("PATCH", DOCUMENT_PATH),
    ("DELETE", DOCUMENT_PATH),
];

pub fn build_router(state: Arc<AppState>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers([REQUEST_ID_HEADER]);

    Router::new()
        .route("/health", get(handlers::health))
        .route("/ready", get(handlers::ready))
        .route(SCHEMA_PATH, put(handlers::put_schema))
        .route(SCHEMA_PATH, get(handlers::get_schema))
        .route(COLLECTION_PATH, post(handlers::create_document))
        .route(COLLECTION_PATH, get(handlers::list_documents))
        .route(QUERY_PATH, post(handlers::query_documents))
        .route(SEARCH_PATH, post(handlers::search_documents))
        .route(DOCUMENT_PATH, get(handlers::get_document))
        .route(DOCUMENT_PATH, put(handlers::replace_document))
        .route(DOCUMENT_PATH, patch(handlers::patch_document))
        .route(DOCUMENT_PATH, delete(handlers::delete_document))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ))
        .layer(middleware::from_fn(request_id))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

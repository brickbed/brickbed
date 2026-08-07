//! Request extractors: the JSON error shape, and authorization that runs
//! *before* the body is parsed.
//!
//! Axum runs extractors in argument order and a rejection short-circuits the
//! rest, so putting [`Guard`]/[`ApiKey`] ahead of [`AppJson`] in a handler
//! signature is what makes "authorize first" structural rather than a habit.
//! With the check inside the handler body it ran *after* deserialization, and
//! an anonymous caller got a 422 about our field names instead of a 401.

use std::marker::PhantomData;
use std::sync::Arc;

use axum::async_trait;
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use axum::response::IntoResponse;
use axum::Json;
use serde::de::DeserializeOwned;

use crate::error::AppError;
use crate::handlers::AppState;
use crate::jwt::Principal;
use crate::rules::{self, Access, Op};

/// Drop-in replacement for `Json<T>` in handler signatures.
///
/// Axum's own `Json` rejects a malformed body with a **plain-text** 400/422,
/// the one response in the API that is not `{"error": "..."}` — clients doing
/// `res.json()` on failures throw on it. This keeps the status and supplies our
/// body shape.
pub struct AppJson<T>(pub T);

#[async_trait]
impl<S, T> FromRequest<S> for AppJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(AppJson(value)),
            // Keep axum's status (400 for malformed JSON, 422 for a body that
            // parses but does not fit the type) and its message, which names
            // the offending field.
            Err(rejection) => Err(AppError::Rejection(
                rejection.status(),
                rejection.body_text(),
            )),
        }
    }
}

impl<T> IntoResponse for AppJson<T>
where
    Json<T>: IntoResponse,
{
    fn into_response(self) -> axum::response::Response {
        Json(self.0).into_response()
    }
}

/// The operation a handler performs, named in its signature via [`Guard`].
pub trait GuardOp {
    const OP: Op;
}

pub struct ReadOp;
pub struct CreateOp;
pub struct UpdateOp;
pub struct DeleteOp;

impl GuardOp for ReadOp {
    const OP: Op = Op::Read;
}
impl GuardOp for CreateOp {
    const OP: Op = Op::Create;
}
impl GuardOp for UpdateOp {
    const OP: Op = Op::Update;
}
impl GuardOp for DeleteOp {
    const OP: Op = Op::Delete;
}

/// Collection-level authorization, resolved before the handler runs.
///
/// Yields the [`Access`], which for an owner rule is [`Access::OwnerScoped`] and
/// still owes a per-document check — the type is what stops a handler from
/// treating an owner rule as a blanket yes.
///
/// This resolution is a **fast-path rejection only**. Rules can be tightened by
/// a concurrent schema push between here and the write, so write handlers carry
/// the [`Principal`] through to a precondition that re-resolves the rule inside
/// the write lock; that one is authoritative. Reads have nothing to race — they
/// are answered from the same snapshot they were authorized against.
pub struct Guard<G: GuardOp>(pub Access, pub Principal, pub PhantomData<G>);

#[async_trait]
impl<G: GuardOp> FromRequestParts<Arc<AppState>> for Guard<G> {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let principal = principal_of(parts);
        let (project, collection) = collection_route(parts.uri.path()).ok_or(AppError::NotFound)?;

        // API keys bypass rules, so they never pay for the schema read.
        if principal.is_api_key() {
            return Ok(Guard(Access::Granted, principal, PhantomData));
        }

        let schema = state.db.get_schema(project).await?;
        let rules = schema.as_ref().and_then(|s| s.rules(collection));
        let access = rules::collection(&principal, rules, G::OP)?;
        Ok(Guard(access, principal, PhantomData))
    }
}

/// Requires a trusted server-side credential and yields its grant (`"*"` or a
/// project name). Project configuration is never end-user reachable: rules
/// scope collections, not the schema that defines them.
pub struct ApiKey(pub String);

#[async_trait]
impl FromRequestParts<Arc<AppState>> for ApiKey {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        match principal_of(parts) {
            Principal::ApiKey { project } => Ok(ApiKey(project)),
            Principal::Anonymous => Err(AppError::Unauthorized),
            Principal::EndUser(_) => Err(AppError::Forbidden),
        }
    }
}

/// The principal the auth middleware established. Its absence can only mean a
/// routing mistake that skipped the middleware, so it degrades to anonymous
/// (which every rule denies) rather than trusting the request.
fn principal_of(parts: &Parts) -> Principal {
    parts
        .extensions
        .get::<Principal>()
        .cloned()
        .unwrap_or(Principal::Anonymous)
}

/// `(project, collection)` of a `/v1/{project}/{collection}/...` path.
fn collection_route(path: &str) -> Option<(&str, &str)> {
    let mut segments = path.strip_prefix("/v1/")?.split('/');
    let project = segments.next().filter(|s| !s.is_empty())?;
    let collection = segments.next().filter(|s| !s.is_empty())?;
    Some((project, collection))
}

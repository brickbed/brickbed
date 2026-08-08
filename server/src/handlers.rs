use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Arc;

use crate::db::{Db, Document, Filter, PreconditionCtx, SearchParams};
use crate::error::AppError;
use crate::extract::{ApiKey, AppJson, CreateOp, DeleteOp, Guard, ReadOp, UpdateOp};
use crate::jwt::{HttpJwksFetcher, JwksCache, Principal};
use crate::keybroker::KeyBroker;
use crate::rules::{self, Access, Op};
use crate::schema::ProjectSchema;
use crate::validate::check_names;

/// How much deeper than the requested page to retrieve when an owner rule can
/// drop hits after retrieval. Mirrors `db::OVERFETCH`.
const OVERFETCH: usize = 4;

/// The authoritative authorization decision, run inside the write lock.
///
/// Resolves the collection rule from the schema as it stands at write time
/// rather than trusting the `Access` computed when the request was admitted: a
/// concurrent schema push can tighten a rule in between, and a captured
/// decision would still let the write through.
fn authorize(principal: &Principal, ctx: PreconditionCtx<'_>, op: Op) -> bool {
    let rules = ctx.collection.and_then(|c| c.rules.as_ref());
    match rules::collection(principal, rules, op) {
        Ok(access) => access.permit(op, ctx.existing, ctx.next).is_ok(),
        Err(_) => false,
    }
}

pub struct AppState {
    pub db: Db,
    /// API key -> granted project ("*" grants all).
    pub api_keys: HashMap<String, String>,
    /// Verifier for instance-derived keys; `None` disables them.
    pub keybroker: Option<KeyBroker>,
    /// Signing keys of the configured identity providers.
    pub jwks: JwksCache<HttpJwksFetcher>,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub data: Vec<Document>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct QueryRequest {
    pub index: String,
    pub params: Map<String, Value>,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    /// Text query, for `text` and `hybrid` modes.
    pub query: Option<String>,
    /// Query vector, for `vector` and `hybrid` modes.
    pub vector: Option<Vec<f32>>,
    /// Index for a single-arm mode; ambiguous (so rejected) in `hybrid`.
    pub index: Option<String>,
    /// Search index for the text arm of a `hybrid` search.
    #[serde(rename = "textIndex")]
    pub text_index: Option<String>,
    /// Vector index for the vector arm of a `hybrid` search.
    #[serde(rename = "vectorIndex")]
    pub vector_index: Option<String>,
    /// `text` | `vector` | `hybrid`; inferred from the body when absent.
    pub mode: Option<String>,
    pub limit: Option<usize>,
    /// `{index, params}` equality predicate applied to hits after retrieval.
    pub filter: Option<Value>,
}

#[derive(Debug, PartialEq)]
enum SearchMode {
    Text,
    Vector,
    Hybrid,
}

impl SearchMode {
    fn name(&self) -> &'static str {
        match self {
            SearchMode::Text => "text",
            SearchMode::Vector => "vector",
            SearchMode::Hybrid => "hybrid",
        }
    }
}

/// Explicit `mode` wins; otherwise the body decides, and asking for both a
/// query and a vector means hybrid.
fn resolve_mode(
    mode: Option<&str>,
    has_query: bool,
    has_vector: bool,
) -> Result<SearchMode, AppError> {
    match mode {
        // An explicit single-arm mode with the other arm's input supplied is
        // contradictory; guessing which one the caller meant misroutes silently.
        Some("text") if has_vector => Err(AppError::BadRequest(
            "\"vector\" provided but mode is \"text\"; drop one of them".to_string(),
        )),
        Some("vector") if has_query => Err(AppError::BadRequest(
            "\"query\" provided but mode is \"vector\"; drop one of them".to_string(),
        )),
        Some("text") => Ok(SearchMode::Text),
        Some("vector") => Ok(SearchMode::Vector),
        Some("hybrid") => Ok(SearchMode::Hybrid),
        Some(other) => Err(AppError::BadRequest(format!(
            "unknown search mode {:?}, expected \"text\", \"vector\" or \"hybrid\"",
            other
        ))),
        None => match (has_query, has_vector) {
            (true, false) => Ok(SearchMode::Text),
            (false, true) => Ok(SearchMode::Vector),
            (true, true) => Ok(SearchMode::Hybrid),
            (false, false) => Err(AppError::BadRequest(
                "search requires \"query\" or \"vector\"".to_string(),
            )),
        },
    }
}

/// Which index each arm uses. `index` names the index of a single-arm search;
/// hybrid runs two arms at once, so it takes `textIndex`/`vectorIndex` and
/// rejects the ambiguous `index`.
fn resolve_indexes(
    mode: &SearchMode,
    body: &SearchRequest,
) -> Result<(Option<String>, Option<String>), AppError> {
    let named = |field: &str| {
        AppError::BadRequest(format!(
            "{:?} only applies to hybrid search; name the index with \"index\"",
            field
        ))
    };
    match mode {
        SearchMode::Hybrid => {
            if body.index.is_some() {
                return Err(AppError::BadRequest(
                    "\"index\" is ambiguous in hybrid search; use \"textIndex\" and \"vectorIndex\""
                        .to_string(),
                ));
            }
            Ok((body.text_index.clone(), body.vector_index.clone()))
        }
        SearchMode::Text => {
            if body.vector_index.is_some() {
                return Err(named("vectorIndex"));
            }
            if body.text_index.is_some() {
                return Err(named("textIndex"));
            }
            Ok((body.index.clone(), None))
        }
        SearchMode::Vector => {
            if body.text_index.is_some() {
                return Err(named("textIndex"));
            }
            if body.vector_index.is_some() {
                return Err(named("vectorIndex"));
            }
            Ok((None, body.index.clone()))
        }
    }
}

/// Parse the `{index, params}` filter. Hand-parsed rather than typed so a
/// malformed filter is a 400 in our JSON error shape, not axum's plain-text
/// body rejection.
fn parse_filter(filter: &Value) -> Result<(String, Map<String, Value>), AppError> {
    let obj = filter
        .as_object()
        .ok_or_else(|| AppError::BadRequest("filter must be an object".to_string()))?;
    let index = obj
        .get("index")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest("filter needs an \"index\" name".to_string()))?;
    let params = obj
        .get("params")
        .and_then(Value::as_object)
        .ok_or_else(|| AppError::BadRequest("filter needs a \"params\" object".to_string()))?;
    Ok((index.to_string(), params.clone()))
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub data: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
}

fn clamp_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(100).clamp(1, 1000)
}

/// Search returns a ranked top-k, so it defaults much smaller than a list page.
fn clamp_search_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(10).clamp(1, 1000)
}

pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
    })
}

pub async fn ready(State(state): State<Arc<AppState>>) -> Result<Json<HealthResponse>, AppError> {
    if state.db.health_check().await.is_err() {
        // `IntoResponse` records this with the request ID and a fixed safe
        // classification. The storage error may contain a bucket or path.
        return Err(AppError::Unavailable(
            "database readiness check failed".to_string(),
        ));
    }
    Ok(Json(HealthResponse {
        status: "ready".to_string(),
    }))
}

pub async fn create_document(
    State(state): State<Arc<AppState>>,
    Guard(_access, principal, _): Guard<CreateOp>,
    Path((project, collection)): Path<(String, String)>,
    AppJson(body): AppJson<Map<String, Value>>,
) -> Result<(StatusCode, Json<Document>), AppError> {
    check_names(&project, &collection, None)?;
    // Guard's Access is advisory for writes: the authoritative decision —
    // including doc[owner] == identity on the submitted body — happens in
    // `authorize` under the write lock, against the rule in force at commit.
    let allowed = |ctx: PreconditionCtx<'_>| authorize(&principal, ctx, Op::Create);
    let doc = state
        .db
        .insert_checked(&project, &collection, body, Some(&allowed))
        .await?;
    Ok((StatusCode::CREATED, Json(doc)))
}

pub async fn get_document(
    State(state): State<Arc<AppState>>,
    Guard(access, _principal, _): Guard<ReadOp>,
    Path((project, collection, id)): Path<(String, String, String)>,
) -> Result<Json<Document>, AppError> {
    check_names(&project, &collection, Some(&id))?;

    let doc = state.db.get(&project, &collection, &id).await?;
    access.permit(Op::Read, Some(&doc.data), None)?;
    Ok(Json(doc))
}

pub async fn replace_document(
    State(state): State<Arc<AppState>>,
    Guard(_access, principal, _): Guard<UpdateOp>,
    Path((project, collection, id)): Path<(String, String, String)>,
    AppJson(body): AppJson<Map<String, Value>>,
) -> Result<Json<Document>, AppError> {
    check_names(&project, &collection, Some(&id))?;

    // Authoritative check: the rule is re-derived from the schema as read under
    // the write lock, and applied to the stored document, so neither a rule
    // change nor a transfer of ownership can land between check and write.
    let allowed = |ctx: PreconditionCtx<'_>| authorize(&principal, ctx, Op::Update);
    let doc = state
        .db
        .replace_checked(&project, &collection, &id, body, Some(&allowed))
        .await?;
    Ok(Json(doc))
}

pub async fn patch_document(
    State(state): State<Arc<AppState>>,
    Guard(_access, principal, _): Guard<UpdateOp>,
    Path((project, collection, id)): Path<(String, String, String)>,
    AppJson(body): AppJson<Map<String, Value>>,
) -> Result<Json<Document>, AppError> {
    check_names(&project, &collection, Some(&id))?;

    // The merge happens inside the write lock, so the "resulting document" the
    // rule is checked against is exactly the one being stored — and the rule
    // itself is the one stored at that moment.
    let allowed = |ctx: PreconditionCtx<'_>| authorize(&principal, ctx, Op::Update);
    let doc = state
        .db
        .patch_checked(&project, &collection, &id, body, Some(&allowed))
        .await?;
    Ok(Json(doc))
}

pub async fn delete_document(
    State(state): State<Arc<AppState>>,
    Guard(_access, principal, _): Guard<DeleteOp>,
    Path((project, collection, id)): Path<(String, String, String)>,
) -> Result<StatusCode, AppError> {
    check_names(&project, &collection, Some(&id))?;

    let allowed = |ctx: PreconditionCtx<'_>| authorize(&principal, ctx, Op::Delete);
    state
        .db
        .delete_checked(&project, &collection, &id, Some(&allowed))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_documents(
    State(state): State<Arc<AppState>>,
    Guard(access, _principal, _): Guard<ReadOp>,
    Path((project, collection)): Path<(String, String)>,
    Query(query): Query<ListQuery>,
) -> Result<Json<ListResponse>, AppError> {
    check_names(&project, &collection, None)?;

    let limit = clamp_limit(query.limit);
    let (mut docs, cursor) = state
        .db
        .list(&project, &collection, limit, query.cursor.as_deref())
        .await?;

    // Cursored endpoints do not overfetch: the cursor must stay consistent with
    // what was actually examined, or a later page would skip owned documents.
    // Owner-scoped pages come back short instead.
    rules::retain(&access, &mut docs, |doc| &doc.data);
    Ok(Json(ListResponse { data: docs, cursor }))
}

pub async fn query_documents(
    State(state): State<Arc<AppState>>,
    Guard(access, _principal, _): Guard<ReadOp>,
    Path((project, collection)): Path<(String, String)>,
    AppJson(body): AppJson<QueryRequest>,
) -> Result<Json<ListResponse>, AppError> {
    check_names(&project, &collection, None)?;

    let limit = clamp_limit(body.limit);
    let (mut docs, cursor) = state
        .db
        .query(
            &project,
            &collection,
            &body.index,
            &body.params,
            limit,
            body.cursor.as_deref(),
        )
        .await?;

    rules::retain(&access, &mut docs, |doc| &doc.data);
    Ok(Json(ListResponse { data: docs, cursor }))
}

pub async fn search_documents(
    State(state): State<Arc<AppState>>,
    Guard(access, _principal, _): Guard<ReadOp>,
    Path((project, collection)): Path<(String, String)>,
    AppJson(body): AppJson<SearchRequest>,
) -> Result<Json<SearchResponse>, AppError> {
    check_names(&project, &collection, None)?;

    let limit = clamp_search_limit(body.limit);
    // Search has no cursor, so overfetching to refill an owner-filtered top-k
    // is safe here in a way it is not for `list`/`_query`.
    let retrieve = match access {
        Access::OwnerScoped(_) => limit.saturating_mul(OVERFETCH),
        Access::Granted => limit,
    };
    let mode = resolve_mode(
        body.mode.as_deref(),
        body.query.is_some(),
        body.vector.is_some(),
    )?;
    let (text_index, vector_index) = resolve_indexes(&mode, &body)?;
    let filter = body.filter.as_ref().map(parse_filter).transpose()?;

    // Each mode needs its own inputs, whether they were inferred or named.
    let needs_query = matches!(mode, SearchMode::Text | SearchMode::Hybrid);
    let needs_vector = matches!(mode, SearchMode::Vector | SearchMode::Hybrid);

    let query = match (needs_query, body.query.as_deref()) {
        (true, None) => {
            return Err(AppError::BadRequest(format!(
                "{} search requires \"query\"",
                mode.name()
            )))
        }
        (true, some) => some,
        (false, _) => None,
    };
    let vector = match (needs_vector, body.vector.as_deref()) {
        (true, None) => {
            return Err(AppError::BadRequest(format!(
                "{} search requires \"vector\"",
                mode.name()
            )))
        }
        (true, some) => some,
        (false, _) => None,
    };
    if let Some(vector) = vector {
        // JSON numbers outside f32 range land here as infinities.
        if let Some(i) = vector.iter().position(|c| !c.is_finite()) {
            return Err(AppError::BadRequest(format!(
                "vector[{}] is not a finite 32-bit float",
                i
            )));
        }
    }

    let mut hits = state
        .db
        .search(SearchParams {
            project: &project,
            collection: &collection,
            query,
            vector,
            text_index: text_index.as_deref(),
            vector_index: vector_index.as_deref(),
            filter: filter.as_ref().map(|(index, params)| Filter {
                index: index.as_str(),
                params,
            }),
            limit: retrieve,
        })
        .await?;

    rules::retain(&access, &mut hits, |(doc, _)| &doc.data);
    hits.truncate(limit);

    // Documents serialize flat, so the score joins them as another field.
    let data = hits
        .into_iter()
        .map(|(doc, score)| {
            let mut value = serde_json::to_value(doc)
                .map_err(|e| AppError::Internal(format!("Serialization error: {}", e)))?;
            if let Some(obj) = value.as_object_mut() {
                obj.insert("_score".to_string(), json!(score));
            }
            Ok(value)
        })
        .collect::<Result<Vec<Value>, AppError>>()?;

    Ok(Json(SearchResponse { data }))
}

pub async fn put_schema(
    State(state): State<Arc<AppState>>,
    ApiKey(grant): ApiKey,
    Path(project): Path<String>,
    AppJson(schema): AppJson<ProjectSchema>,
) -> Result<Json<ProjectSchema>, AppError> {
    if !crate::validate::valid_name(&project) {
        return Err(AppError::Validation(format!(
            "invalid project name: {:?}",
            project
        )));
    }
    crate::validate::check_schema_names(&schema)?;
    crate::validate::check_reserved_schema_fields(&schema)?;
    crate::validate::check_vector_indexes(&schema)?;
    crate::validate::check_embed_sources(&schema)?;
    crate::validate::check_auth_config(&schema)?;

    // Rewriting `auth` repoints who may mint identities for this project, so it
    // takes an instance-wide key. A project-scoped key can still push ordinary
    // schema changes alongside an unchanged `auth` block.
    //
    // The comparison runs inside the write lock, against the schema actually
    // being replaced. Comparing against a separately-read copy would let a
    // concurrent admin push land in between, so a stale payload that passed the
    // gate could revert the very change it was compared against.
    let allowed = |stored: Option<&ProjectSchema>| {
        if grant == "*" {
            return true;
        }
        if crate::validate::changes_auth_config(&schema, stored) {
            tracing::warn!(
                "refused auth config change on {:?} from a project-scoped key",
                project
            );
            return false;
        }
        true
    };

    state
        .db
        .put_schema_checked(&project, &schema, Some(&allowed))
        .await?;
    Ok(Json(schema))
}

pub async fn get_schema(
    State(state): State<Arc<AppState>>,
    ApiKey(_grant): ApiKey,
    Path(project): Path<String>,
) -> Result<Json<ProjectSchema>, AppError> {
    let schema = state
        .db
        .get_schema(&project)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(schema))
}

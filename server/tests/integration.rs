use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tower::ServiceExt;

use brickbed_server::build_router;
use brickbed_server::config::{Config, StorageBackend};
use brickbed_server::db::Db;
use brickbed_server::embed::{EmbeddingProvider, MockProvider};
use brickbed_server::handlers::AppState;
use brickbed_server::jwt::{HttpJwksFetcher, JwksCache};
use brickbed_server::keybroker::KeyBroker;

const KEY: &str = "test-key";
const PROJECT: &str = "testproj";
const INSTANCE: &str = "test-instance";
const SECRET: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

async fn test_app() -> (Router, tempfile::TempDir) {
    test_app_with(None, None).await
}

async fn test_app_with_broker(keybroker: Option<KeyBroker>) -> (Router, tempfile::TempDir) {
    test_app_with(keybroker, None).await
}

/// App with an explicit embedding provider. `None` leaves embed-on-write off,
/// which is the configuration every other test runs under.
async fn test_app_with_embedder(
    embedder: Option<Arc<dyn EmbeddingProvider>>,
) -> (Router, tempfile::TempDir) {
    test_app_with(None, embedder).await
}

async fn test_app_with(
    keybroker: Option<KeyBroker>,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
) -> (Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        host: "127.0.0.1".to_string(),
        port: 0,
        storage: StorageBackend::Local {
            path: dir.path().to_str().unwrap().to_string(),
        },
        db_path: "test".to_string(),
        api_keys: HashMap::from([
            (KEY.to_string(), PROJECT.to_string()),
            ("admin-key".to_string(), "*".to_string()),
        ]),
        keybroker,
        embeddings: None,
    };
    let db = Db::open_with_embedder(&config, embedder).await.unwrap();
    let state = Arc::new(AppState {
        db,
        api_keys: config.api_keys.clone(),
        keybroker: config.keybroker.clone(),
        jwks: JwksCache::new(HttpJwksFetcher::new()),
    });
    (build_router(state), dir)
}

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    key: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(path);
    if let Some(key) = key {
        req = req.header("Authorization", format!("Bearer {}", key));
    }
    let req = match body {
        Some(v) => req
            .header("Content-Type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };

    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "non-JSON response body (status {}): {:?} ({})",
                status,
                String::from_utf8_lossy(&bytes),
                e
            )
        })
    };
    (status, value)
}

fn error_message(body: &Value) -> &str {
    body["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("missing v1 error message: {}", body))
}

fn error_code(body: &Value) -> &str {
    body["error"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("missing v1 error code: {}", body))
}

fn schema_body() -> Value {
    json!({
        "collections": {
            "posts": {
                "fields": {
                    "title": {"type": "string"},
                    "slug": {"type": "string"},
                    "status": {"type": "union", "variants": [
                        {"type": "literal", "value": "draft"},
                        {"type": "literal", "value": "published"}
                    ]},
                    "publishedAt": {"type": "optional", "inner": {"type": "number"}},
                    "tags": {"type": "array", "items": {"type": "string"}}
                },
                "indexes": [
                    {"name": "by_slug", "fields": ["slug"]},
                    {"name": "by_status", "fields": ["status", "publishedAt"]}
                ]
            }
        }
    })
}

/// Schema with a BM25 index over the text fields of `posts`.
fn search_schema_body() -> Value {
    let mut schema = schema_body();
    schema["collections"]["posts"]["fields"]["body"] =
        json!({"type": "optional", "inner": {"type": "string"}});
    schema["collections"]["posts"]["searchIndexes"] =
        json!([{"name": "search", "fields": ["title", "body", "tags"]}]);
    schema
}

/// Schema with a 3-dimension vector field and a cosine index over it, kept on
/// top of the BM25 index so both search modes are exercised together.
fn vector_schema_body() -> Value {
    let mut schema = search_schema_body();
    schema["collections"]["posts"]["fields"]["embedding"] =
        json!({"type": "optional", "inner": {"type": "vector", "dims": 3}});
    schema["collections"]["posts"]["vectorIndexes"] = json!([
        {"name": "by_embedding", "field": "embedding", "metric": "cosine", "dims": 3}
    ]);
    schema
}

/// Post whose body mentions "rust" `hits` times, padded to a constant token
/// count so BM25 ranks a set of these strictly by term frequency.
fn ranked_post(slug: &str, status: &str, hits: usize, embedding: Value) -> Value {
    let mut words = vec!["rust"; hits];
    words.extend(vec!["filler"; 10 - hits]);
    json!({
        "title": format!("Post {}", slug),
        "body": words.join(" "),
        "slug": slug,
        "status": status,
        "tags": [],
        "embedding": embedding
    })
}

/// Post carrying an embedding. `embedding` is inserted verbatim so tests can
/// pass malformed values.
fn vector_post(slug: &str, embedding: Value) -> Value {
    let mut doc = searchable_post(slug, &format!("Post {}", slug), "Body text.");
    doc["embedding"] = embedding;
    doc
}

fn searchable_post(slug: &str, title: &str, body: &str) -> Value {
    json!({
        "title": title,
        "body": body,
        "slug": slug,
        "status": "published",
        "tags": []
    })
}

async fn push_schema(app: &Router, schema: Value) {
    let (status, _) = send(
        app,
        "PUT",
        &format!("/v1/{}/_schema", PROJECT),
        Some(KEY),
        Some(schema),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

async fn create_post(app: &Router, doc: Value) -> String {
    let (status, created) = send(
        app,
        "POST",
        &format!("/v1/{}/posts", PROJECT),
        Some(KEY),
        Some(doc),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    created["_id"].as_str().unwrap().to_string()
}

async fn search(app: &Router, body: Value) -> (StatusCode, Value) {
    send(
        app,
        "POST",
        &format!("/v1/{}/posts/_search", PROJECT),
        Some(KEY),
        Some(body),
    )
    .await
}

/// Slug and `_score` of every hit, best-scoring first.
async fn search_hits(app: &Router, body: Value) -> Vec<(String, f64)> {
    let (status, res) = search(app, body).await;
    assert_eq!(status, StatusCode::OK);
    res["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            (
                d["slug"].as_str().unwrap().to_string(),
                d["_score"].as_f64().expect("_score on every hit"),
            )
        })
        .collect()
}

/// Slugs of a `_search` response, best-scoring first.
async fn search_slugs(app: &Router, body: Value) -> Vec<String> {
    search_hits(app, body)
        .await
        .into_iter()
        .map(|(slug, _)| slug)
        .collect()
}

fn post_doc(slug: &str, status: &str, published_at: Option<u64>) -> Value {
    let mut doc = json!({
        "title": format!("Post {}", slug),
        "slug": slug,
        "status": status,
        "tags": []
    });
    if let Some(ts) = published_at {
        doc["publishedAt"] = json!(ts);
    }
    doc
}

#[tokio::test]
async fn health_needs_no_auth() {
    let (app, _dir) = test_app().await;
    let (status, body) = send(&app, "GET", "/health", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    let (status, body) = send(&app, "GET", "/ready", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ready");
}

#[tokio::test]
async fn auth_is_enforced() {
    let (app, _dir) = test_app().await;

    let path = format!("/v1/{}/posts", PROJECT);
    let (status, _) = send(&app, "GET", &path, None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = send(&app, "GET", &path, Some("wrong-key"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Valid key, wrong project.
    let (status, _) = send(&app, "GET", "/v1/otherproj/posts", Some(KEY), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Wildcard key reaches any project.
    let (status, _) = send(&app, "GET", &path, Some("admin-key"), None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn crud_roundtrip() {
    let (app, _dir) = test_app().await;
    let base = format!("/v1/{}/posts", PROJECT);

    let (status, created) = send(
        &app,
        "POST",
        &base,
        Some(KEY),
        Some(post_doc("hello", "draft", None)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["_id"].as_str().unwrap().to_string();
    assert!(created["_createdAt"].is_number());

    let (status, fetched) = send(&app, "GET", &format!("{}/{}", base, id), Some(KEY), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["slug"], "hello");

    let (status, patched) = send(
        &app,
        "PATCH",
        &format!("{}/{}", base, id),
        Some(KEY),
        Some(json!({"title": "Updated"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(patched["title"], "Updated");
    assert_eq!(patched["slug"], "hello");

    let (status, listed) = send(&app, "GET", &base, Some(KEY), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);

    let (status, _) = send(&app, "DELETE", &format!("{}/{}", base, id), Some(KEY), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = send(&app, "GET", &format!("{}/{}", base, id), Some(KEY), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reserved_document_fields_are_rejected_without_corrupting_data() {
    let (app, _dir) = test_app().await;
    let base = format!("/v1/{}/posts", PROJECT);

    for field in ["_id", "_createdAt", "_updatedAt", "_score"] {
        let mut body = post_doc("reserved", "draft", None);
        body.as_object_mut()
            .unwrap()
            .insert(field.to_string(), json!(1));
        let (status, error) = send(&app, "POST", &base, Some(KEY), Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "accepted {field}");
        assert!(error_message(&error).contains("reserved"));
    }

    let (status, created) = send(
        &app,
        "POST",
        &base,
        Some(KEY),
        Some(post_doc("safe", "draft", None)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["_id"].as_str().unwrap();

    for method in ["PUT", "PATCH"] {
        let (status, error) = send(
            &app,
            method,
            &format!("{}/{}", base, id),
            Some(KEY),
            Some(json!({"_updatedAt": 0})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{method} accepted metadata"
        );
        assert!(error_message(&error).contains("reserved"));
    }

    let (status, fetched) = send(&app, "GET", &format!("{}/{}", base, id), Some(KEY), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["slug"], "safe");
}

#[tokio::test]
async fn schema_cannot_declare_reserved_document_fields() {
    let (app, _dir) = test_app().await;
    let mut schema = schema_body();
    schema["collections"]["posts"]["fields"]["_id"] = json!({"type": "string"});

    let (status, error) = send(
        &app,
        "PUT",
        &format!("/v1/{}/_schema", PROJECT),
        Some(KEY),
        Some(schema),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error_message(&error).contains("reserved"));
}

#[tokio::test]
async fn invalid_names_rejected() {
    let (app, _dir) = test_app().await;

    // Colon in collection name would escape the key namespace.
    let path = format!("/v1/{}/bad:name", PROJECT);
    let (status, _) = send(&app, "POST", &path, Some(KEY), Some(json!({"a": 1}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Uppercase rejected.
    let path = format!("/v1/{}/Posts", PROJECT);
    let (status, _) = send(&app, "POST", &path, Some(KEY), Some(json!({"a": 1}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Underscore-prefixed collection is reserved.
    let path = format!("/v1/{}/_internal", PROJECT);
    let (status, _) = send(&app, "POST", &path, Some(KEY), Some(json!({"a": 1}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn schema_validation_on_write() {
    let (app, _dir) = test_app().await;
    let base = format!("/v1/{}/posts", PROJECT);

    let (status, _) = send(
        &app,
        "PUT",
        &format!("/v1/{}/_schema", PROJECT),
        Some(KEY),
        Some(schema_body()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, fetched) = send(
        &app,
        "GET",
        &format!("/v1/{}/_schema", PROJECT),
        Some(KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(fetched["collections"]["posts"].is_object());

    // Missing required field (first failure in field order is reported).
    let (status, err) = send(&app, "POST", &base, Some(KEY), Some(json!({"slug": "x"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(err["error"]
        .as_str()
        .unwrap()
        .contains("missing required field"));

    // Wrong union variant.
    let mut bad = post_doc("x", "archived", None);
    bad["status"] = json!("archived");
    let (status, _) = send(&app, "POST", &base, Some(KEY), Some(bad)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Valid doc passes; undeclared extra fields are allowed.
    let mut ok = post_doc("x", "draft", None);
    ok["extraField"] = json!("allowed");
    let (status, _) = send(&app, "POST", &base, Some(KEY), Some(ok)).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn query_by_index() {
    let (app, _dir) = test_app().await;
    let base = format!("/v1/{}/posts", PROJECT);

    let (status, _) = send(
        &app,
        "PUT",
        &format!("/v1/{}/_schema", PROJECT),
        Some(KEY),
        Some(schema_body()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    for (slug, status_field, ts) in [
        ("alpha", "published", Some(100)),
        ("beta", "published", Some(200)),
        ("gamma", "draft", None),
    ] {
        let (status, _) = send(
            &app,
            "POST",
            &base,
            Some(KEY),
            Some(post_doc(slug, status_field, ts)),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    // Exact match on single-field index.
    let (status, res) = send(
        &app,
        "POST",
        &format!("{}/_query", base),
        Some(KEY),
        Some(json!({"index": "by_slug", "params": {"slug": "beta"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let data = res["data"].as_array().unwrap();
    assert_eq!(data.len(), 1);
    assert_eq!(data[0]["slug"], "beta");

    // Prefix match on composite index: status only.
    let (status, res) = send(
        &app,
        "POST",
        &format!("{}/_query", base),
        Some(KEY),
        Some(json!({"index": "by_status", "params": {"status": "published"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(res["data"].as_array().unwrap().len(), 2);

    // Unknown index -> 400.
    let (status, _) = send(
        &app,
        "POST",
        &format!("{}/_query", base),
        Some(KEY),
        Some(json!({"index": "nope", "params": {"slug": "beta"}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Params not forming an index prefix -> 400.
    let (status, _) = send(
        &app,
        "POST",
        &format!("{}/_query", base),
        Some(KEY),
        Some(json!({"index": "by_status", "params": {"publishedAt": 100}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn index_entries_follow_updates_and_deletes() {
    let (app, _dir) = test_app().await;
    let base = format!("/v1/{}/posts", PROJECT);

    let (status, _) = send(
        &app,
        "PUT",
        &format!("/v1/{}/_schema", PROJECT),
        Some(KEY),
        Some(schema_body()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, created) = send(
        &app,
        "POST",
        &base,
        Some(KEY),
        Some(post_doc("old-slug", "draft", None)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["_id"].as_str().unwrap().to_string();

    // Patch the indexed field: old entry must disappear, new one appear.
    let (status, _) = send(
        &app,
        "PATCH",
        &format!("{}/{}", base, id),
        Some(KEY),
        Some(json!({"slug": "new-slug"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let query = |slug: &str| json!({"index": "by_slug", "params": {"slug": slug}});
    let (_, res) = send(
        &app,
        "POST",
        &format!("{}/_query", base),
        Some(KEY),
        Some(query("old-slug")),
    )
    .await;
    assert_eq!(res["data"].as_array().unwrap().len(), 0);
    let (_, res) = send(
        &app,
        "POST",
        &format!("{}/_query", base),
        Some(KEY),
        Some(query("new-slug")),
    )
    .await;
    assert_eq!(res["data"].as_array().unwrap().len(), 1);

    // Delete removes the index entry.
    let (status, _) = send(&app, "DELETE", &format!("{}/{}", base, id), Some(KEY), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, res) = send(
        &app,
        "POST",
        &format!("{}/_query", base),
        Some(KEY),
        Some(query("new-slug")),
    )
    .await;
    assert_eq!(res["data"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn schema_push_backfills_existing_docs() {
    let (app, _dir) = test_app().await;
    let base = format!("/v1/{}/posts", PROJECT);

    // Insert before any schema exists (no indexes yet).
    let (status, _) = send(
        &app,
        "POST",
        &base,
        Some(KEY),
        Some(post_doc("early", "draft", None)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Push schema afterwards; existing docs get indexed.
    let (status, _) = send(
        &app,
        "PUT",
        &format!("/v1/{}/_schema", PROJECT),
        Some(KEY),
        Some(schema_body()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, res) = send(
        &app,
        "POST",
        &format!("{}/_query", base),
        Some(KEY),
        Some(json!({"index": "by_slug", "params": {"slug": "early"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(res["data"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn query_pagination() {
    let (app, _dir) = test_app().await;
    let base = format!("/v1/{}/posts", PROJECT);

    let (status, _) = send(
        &app,
        "PUT",
        &format!("/v1/{}/_schema", PROJECT),
        Some(KEY),
        Some(schema_body()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    for i in 0..5 {
        let (status, _) = send(
            &app,
            "POST",
            &base,
            Some(KEY),
            Some(post_doc(&format!("post-{}", i), "published", Some(i))),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    let (_, page1) = send(
        &app,
        "POST",
        &format!("{}/_query", base),
        Some(KEY),
        Some(json!({"index": "by_status", "params": {"status": "published"}, "limit": 2})),
    )
    .await;
    assert_eq!(page1["data"].as_array().unwrap().len(), 2);
    let cursor = page1["cursor"]
        .as_str()
        .expect("cursor on truncated page")
        .to_string();

    let (_, page2) = send(
        &app,
        "POST",
        &format!("{}/_query", base),
        Some(KEY),
        Some(json!({
            "index": "by_status",
            "params": {"status": "published"},
            "limit": 2,
            "cursor": cursor
        })),
    )
    .await;
    assert_eq!(page2["data"].as_array().unwrap().len(), 2);

    // Pages must not overlap; publishedAt ordering means post-0,1 then post-2,3.
    let slug = |page: &Value, i: usize| page["data"][i]["slug"].as_str().unwrap().to_string();
    assert_eq!(slug(&page1, 0), "post-0");
    assert_eq!(slug(&page1, 1), "post-1");
    assert_eq!(slug(&page2, 0), "post-2");
    assert_eq!(slug(&page2, 1), "post-3");
}

#[tokio::test]
async fn schema_rejects_unusable_index_names() {
    let (app, _dir) = test_app().await;
    let path = format!("/v1/{}/_schema", PROJECT);

    let push = |schema: Value| {
        let app = app.clone();
        let path = path.clone();
        async move { send(&app, "PUT", &path, Some(KEY), Some(schema)).await.0 }
    };

    // A `:` or NUL in an index name would let its entries fall inside another
    // index's scan range.
    let mut schema = search_schema_body();
    schema["collections"]["posts"]["indexes"][0]["name"] = json!("by:slug");
    assert_eq!(push(schema).await, StatusCode::BAD_REQUEST);

    let mut schema = search_schema_body();
    schema["collections"]["posts"]["searchIndexes"][0]["name"] = json!("search\u{0000}x");
    assert_eq!(push(schema).await, StatusCode::BAD_REQUEST);

    // Duplicates share one keyspace but would be counted twice in corpus stats.
    let mut schema = search_schema_body();
    schema["collections"]["posts"]["searchIndexes"] = json!([
        {"name": "search", "fields": ["title"]},
        {"name": "search", "fields": ["body"]}
    ]);
    assert_eq!(push(schema).await, StatusCode::BAD_REQUEST);

    let mut schema = search_schema_body();
    schema["collections"]["posts"]["indexes"][1]["name"] = json!("by_slug");
    assert_eq!(push(schema).await, StatusCode::BAD_REQUEST);

    // Index names are not held to the collection-name charset.
    let mut schema = search_schema_body();
    schema["collections"]["posts"]["indexes"][0]["name"] = json!("bySlug");
    assert_eq!(push(schema).await, StatusCode::OK);
}

#[tokio::test]
async fn search_ranks_by_relevance() {
    let (app, _dir) = test_app().await;
    let base = format!("/v1/{}/posts", PROJECT);
    push_schema(&app, search_schema_body()).await;

    for post in [
        searchable_post(
            "alpha",
            "Rust storage engine",
            "Rust is fast. This rust engine stores rust documents.",
        ),
        searchable_post(
            "beta",
            "Storage engines compared",
            "A long note about databases, engines, indexes, caching and one mention of rust.",
        ),
        searchable_post("gamma", "Cooking pasta", "Nothing relevant in this post."),
    ] {
        let (status, _) = send(&app, "POST", &base, Some(KEY), Some(post)).await;
        assert_eq!(status, StatusCode::CREATED);
    }

    // More mentions in a shorter document wins.
    let slugs = search_slugs(&app, json!({"query": "rust"})).await;
    assert_eq!(slugs, vec!["alpha", "beta"]);

    // Stemming: "engines" in the query matches "engine" in the documents.
    let slugs = search_slugs(&app, json!({"query": "engines"})).await;
    assert_eq!(slugs.len(), 2);

    // Scores are present, positive and descending.
    let (status, res) = send(
        &app,
        "POST",
        &format!("{}/_search", base),
        Some(KEY),
        Some(json!({"query": "rust storage"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let data = res["data"].as_array().unwrap();
    assert_eq!(data[0]["slug"], "alpha");
    let scores: Vec<f64> = data.iter().map(|d| d["_score"].as_f64().unwrap()).collect();
    assert!(scores.iter().all(|s| *s > 0.0), "scores: {:?}", scores);
    assert!(
        scores.windows(2).all(|w| w[0] >= w[1]),
        "scores: {:?}",
        scores
    );

    // Explicit index name resolves the same index as the default.
    let slugs = search_slugs(&app, json!({"query": "rust", "index": "search"})).await;
    assert_eq!(slugs, vec!["alpha", "beta"]);

    // limit caps the result set.
    let slugs = search_slugs(&app, json!({"query": "rust", "limit": 1})).await;
    assert_eq!(slugs, vec!["alpha"]);
}

#[tokio::test]
async fn search_follows_updates_and_deletes() {
    let (app, _dir) = test_app().await;
    let base = format!("/v1/{}/posts", PROJECT);
    push_schema(&app, search_schema_body()).await;

    let (status, created) = send(
        &app,
        "POST",
        &base,
        Some(KEY),
        Some(searchable_post(
            "solo",
            "Gardening tips",
            "Mulch and compost.",
        )),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["_id"].as_str().unwrap().to_string();

    assert_eq!(
        search_slugs(&app, json!({"query": "gardening"}))
            .await
            .len(),
        1
    );
    assert!(search_slugs(&app, json!({"query": "rust"}))
        .await
        .is_empty());

    // Patch replaces the indexed text: old postings must go, new ones appear.
    let (status, _) = send(
        &app,
        "PATCH",
        &format!("{}/{}", base, id),
        Some(KEY),
        Some(json!({"title": "Rust notes", "body": "Rust everywhere."})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(search_slugs(&app, json!({"query": "gardening"}))
        .await
        .is_empty());
    assert_eq!(search_slugs(&app, json!({"query": "rust"})).await.len(), 1);

    // Replace (PUT) is on the same path.
    let (status, _) = send(
        &app,
        "PUT",
        &format!("{}/{}", base, id),
        Some(KEY),
        Some(searchable_post(
            "solo",
            "Baking bread",
            "Sourdough starter.",
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(search_slugs(&app, json!({"query": "rust"}))
        .await
        .is_empty());
    assert_eq!(
        search_slugs(&app, json!({"query": "sourdough"}))
            .await
            .len(),
        1
    );

    // Delete drops the document from the index entirely.
    let (status, _) = send(&app, "DELETE", &format!("{}/{}", base, id), Some(KEY), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(search_slugs(&app, json!({"query": "sourdough"}))
        .await
        .is_empty());
    assert!(search_slugs(&app, json!({"query": "bread"}))
        .await
        .is_empty());
}

#[tokio::test]
async fn schema_push_backfills_search_index() {
    let (app, _dir) = test_app().await;
    let base = format!("/v1/{}/posts", PROJECT);

    // Documents written before any search index exists.
    for post in [
        searchable_post("early", "Rust in production", "Shipping rust services."),
        searchable_post("other", "Weekend baking", "Sourdough again."),
    ] {
        let (status, _) = send(&app, "POST", &base, Some(KEY), Some(post)).await;
        assert_eq!(status, StatusCode::CREATED);
    }

    push_schema(&app, search_schema_body()).await;
    assert_eq!(
        search_slugs(&app, json!({"query": "rust"})).await,
        vec!["early"]
    );

    // Re-pushing rebuilds from scratch without double-counting.
    push_schema(&app, search_schema_body()).await;
    assert_eq!(
        search_slugs(&app, json!({"query": "rust"})).await,
        vec!["early"]
    );

    // Dropping the search index makes search unavailable again.
    push_schema(&app, schema_body()).await;
    let (status, _) = send(
        &app,
        "POST",
        &format!("{}/_search", base),
        Some(KEY),
        Some(json!({"query": "rust"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn search_empty_and_missing_matches() {
    let (app, _dir) = test_app().await;
    let base = format!("/v1/{}/posts", PROJECT);
    push_schema(&app, search_schema_body()).await;

    // No documents yet.
    assert!(search_slugs(&app, json!({"query": "rust"}))
        .await
        .is_empty());

    let (status, _) = send(
        &app,
        "POST",
        &base,
        Some(KEY),
        Some(searchable_post("alpha", "Rust storage", "Rust documents.")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Empty and stopword-only queries produce no terms, so no matches.
    assert!(search_slugs(&app, json!({"query": ""})).await.is_empty());
    assert!(search_slugs(&app, json!({"query": "   "})).await.is_empty());
    assert!(search_slugs(&app, json!({"query": "the and of"}))
        .await
        .is_empty());

    // Term nobody uses.
    assert!(search_slugs(&app, json!({"query": "kubernetes"}))
        .await
        .is_empty());

    // Partial match still returns the documents that hold the other terms.
    assert_eq!(
        search_slugs(&app, json!({"query": "kubernetes rust"})).await,
        vec!["alpha"]
    );
}

#[tokio::test]
async fn search_rejects_unknown_index_and_missing_schema() {
    let (app, _dir) = test_app().await;
    let base = format!("/v1/{}/posts", PROJECT);
    let search = format!("{}/_search", base);

    // No schema pushed at all.
    let (status, _) = send(
        &app,
        "POST",
        &search,
        Some(KEY),
        Some(json!({"query": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    push_schema(&app, search_schema_body()).await;

    // Unknown search index name.
    let (status, err) = send(
        &app,
        "POST",
        &search,
        Some(KEY),
        Some(json!({"query": "x", "index": "nope"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(err["error"]
        .as_str()
        .unwrap()
        .contains("unknown search index"));

    // An equality index is not a search index.
    let (status, _) = send(
        &app,
        "POST",
        &search,
        Some(KEY),
        Some(json!({"query": "x", "index": "by_slug"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Collection absent from the schema.
    let (status, _) = send(
        &app,
        "POST",
        &format!("/v1/{}/authors/_search", PROJECT),
        Some(KEY),
        Some(json!({"query": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Search is behind the same auth as everything else.
    let (status, _) = send(&app, "POST", &search, None, Some(json!({"query": "x"}))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn vector_search_ranks_by_similarity() {
    let (app, _dir) = test_app().await;
    push_schema(&app, vector_schema_body()).await;

    // Unit axes plus one vector just off the x axis: the ordering against a
    // query of [1, 0, 0] is obvious by construction.
    for (slug, embedding) in [
        ("east", json!([1.0, 0.0, 0.0])),
        ("near-east", json!([0.9, 0.1, 0.0])),
        ("north", json!([0.0, 1.0, 0.0])),
        ("west", json!([-1.0, 0.0, 0.0])),
    ] {
        create_post(&app, vector_post(slug, embedding)).await;
    }
    // A document without the field stays out of the index entirely.
    create_post(
        &app,
        searchable_post("no-vector", "Post no-vector", "Body."),
    )
    .await;

    let hits = search_hits(&app, json!({"vector": [1.0, 0.0, 0.0]})).await;
    let slugs: Vec<&str> = hits.iter().map(|(s, _)| s.as_str()).collect();
    assert_eq!(slugs, vec!["east", "near-east", "north", "west"]);

    let scores: Vec<f64> = hits.iter().map(|(_, s)| *s).collect();
    assert!(
        scores.windows(2).all(|w| w[0] >= w[1]),
        "scores: {:?}",
        scores
    );
    // Cosine similarity: 1 for the same direction, 0 orthogonal, -1 opposite.
    assert!((scores[0] - 1.0).abs() < 1e-6, "scores: {:?}", scores);
    assert!(scores[2].abs() < 1e-6, "scores: {:?}", scores);
    assert!((scores[3] + 1.0).abs() < 1e-6, "scores: {:?}", scores);

    // Explicit mode and index name resolve the same way as the inferred ones.
    assert_eq!(
        search_slugs(
            &app,
            json!({"vector": [1.0, 0.0, 0.0], "mode": "vector", "index": "by_embedding"})
        )
        .await,
        slugs
    );

    // limit caps the result set.
    assert_eq!(
        search_slugs(&app, json!({"vector": [1.0, 0.0, 0.0], "limit": 2})).await,
        vec!["east", "near-east"]
    );

    // Text search over the same collection is unaffected.
    assert_eq!(
        search_slugs(&app, json!({"query": "north"})).await,
        vec!["north"]
    );
}

#[tokio::test]
async fn vector_search_honours_the_dot_metric() {
    let (app, _dir) = test_app().await;
    let mut schema = vector_schema_body();
    schema["collections"]["posts"]["vectorIndexes"][0]["metric"] = json!("dot");
    push_schema(&app, schema).await;

    // Same direction, different magnitudes: cosine would tie them, dot does not.
    create_post(&app, vector_post("short", json!([1.0, 0.0, 0.0]))).await;
    create_post(&app, vector_post("long", json!([4.0, 0.0, 0.0]))).await;

    let hits = search_hits(&app, json!({"vector": [2.0, 0.0, 0.0]})).await;
    assert_eq!(
        hits.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(),
        vec!["long", "short"]
    );
    assert!((hits[0].1 - 8.0).abs() < 1e-6, "hits: {:?}", hits);
    assert!((hits[1].1 - 2.0).abs() < 1e-6, "hits: {:?}", hits);
}

#[tokio::test]
async fn vector_dims_are_validated() {
    let (app, _dir) = test_app().await;
    let base = format!("/v1/{}/posts", PROJECT);
    push_schema(&app, vector_schema_body()).await;

    // Too few components.
    let (status, err) = send(
        &app,
        "POST",
        &base,
        Some(KEY),
        Some(vector_post("short", json!([1.0, 0.0]))),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        error_message(&err).contains("dimensions"),
        "error: {}",
        err["error"]
    );

    // Too many, and non-numeric components.
    for bad in [
        json!([1.0, 0.0, 0.0, 0.0]),
        json!([1.0, "two", 0.0]),
        json!("not an array"),
    ] {
        let (status, _) = send(
            &app,
            "POST",
            &base,
            Some(KEY),
            Some(vector_post("bad", bad)),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    // Omitting an optional vector field is still fine.
    create_post(&app, searchable_post("none", "Post none", "Body.")).await;

    // Query vectors are checked against the index width too.
    let (status, err) = search(&app, json!({"vector": [1.0, 0.0]})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        error_message(&err).contains("2 dimensions"),
        "error: {}",
        err["error"]
    );
}

#[tokio::test]
async fn vector_entries_follow_updates_and_deletes() {
    let (app, _dir) = test_app().await;
    let base = format!("/v1/{}/posts", PROJECT);
    push_schema(&app, vector_schema_body()).await;

    let east = create_post(&app, vector_post("east", json!([1.0, 0.0, 0.0]))).await;
    let north = create_post(&app, vector_post("north", json!([0.0, 1.0, 0.0]))).await;

    let query = json!({"vector": [1.0, 0.0, 0.0]});
    assert_eq!(
        search_slugs(&app, query.clone()).await,
        vec!["east", "north"]
    );

    // Patching the vector re-ranks: north now points along x, east along y.
    let (status, _) = send(
        &app,
        "PATCH",
        &format!("{}/{}", base, north),
        Some(KEY),
        Some(json!({"embedding": [1.0, 0.0, 0.0]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(
        &app,
        "PATCH",
        &format!("{}/{}", base, east),
        Some(KEY),
        Some(json!({"embedding": [0.0, 1.0, 0.0]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        search_slugs(&app, query.clone()).await,
        vec!["north", "east"]
    );

    // Replacing without the field drops the document from the index.
    let (status, _) = send(
        &app,
        "PUT",
        &format!("{}/{}", base, east),
        Some(KEY),
        Some(searchable_post("east", "Post east", "Body.")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(search_slugs(&app, query.clone()).await, vec!["north"]);

    // Delete removes the last entry.
    let (status, _) = send(
        &app,
        "DELETE",
        &format!("{}/{}", base, north),
        Some(KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(search_slugs(&app, query).await.is_empty());
}

#[tokio::test]
async fn vector_cache_reflects_writes() {
    let (app, _dir) = test_app().await;
    push_schema(&app, vector_schema_body()).await;

    create_post(&app, vector_post("north", json!([0.0, 1.0, 0.0]))).await;

    // First search fills the in-memory cache for the index.
    let query = json!({"vector": [1.0, 0.0, 0.0]});
    assert_eq!(search_slugs(&app, query.clone()).await, vec!["north"]);

    // A later insert must be visible to the next search, cache or not.
    create_post(&app, vector_post("east", json!([1.0, 0.0, 0.0]))).await;
    assert_eq!(
        search_slugs(&app, query.clone()).await,
        vec!["east", "north"]
    );

    // And so must a schema push, which rebuilds every entry.
    push_schema(&app, vector_schema_body()).await;
    assert_eq!(search_slugs(&app, query).await, vec!["east", "north"]);
}

#[tokio::test]
async fn schema_push_backfills_vector_index() {
    let (app, _dir) = test_app().await;

    // Written before any schema exists, so nothing validates or indexes them.
    create_post(&app, vector_post("east", json!([1.0, 0.0, 0.0]))).await;
    create_post(&app, vector_post("north", json!([0.0, 1.0, 0.0]))).await;
    create_post(&app, vector_post("malformed", json!([1.0, 0.0]))).await;
    create_post(&app, searchable_post("none", "Post none", "Body.")).await;

    push_schema(&app, vector_schema_body()).await;

    let query = json!({"vector": [1.0, 0.0, 0.0]});
    // The wrong-width document is skipped rather than indexed at the wrong size.
    assert_eq!(
        search_slugs(&app, query.clone()).await,
        vec!["east", "north"]
    );

    // Re-pushing rebuilds from scratch without duplicating entries.
    push_schema(&app, vector_schema_body()).await;
    assert_eq!(
        search_slugs(&app, query.clone()).await,
        vec!["east", "north"]
    );

    // Dropping the vector index makes vector search unavailable again.
    push_schema(&app, search_schema_body()).await;
    let (status, err) = search(&app, query).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        error_message(&err).contains("no vector index"),
        "error: {}",
        err["error"]
    );
}

#[tokio::test]
async fn search_rejects_unknown_vector_index_and_unsupported_modes() {
    let (app, _dir) = test_app().await;
    push_schema(&app, vector_schema_body()).await;
    create_post(&app, vector_post("east", json!([1.0, 0.0, 0.0]))).await;

    let vector = json!([1.0, 0.0, 0.0]);

    // Unknown vector index, and a BM25 index is not a vector index.
    let (status, err) = search(&app, json!({"vector": vector, "index": "nope"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        err["error"]
            .as_str()
            .unwrap()
            .contains("unknown vector index"),
        "error: {}",
        err["error"]
    );
    let (status, _) = search(&app, json!({"vector": vector, "index": "search"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Neither input, an unknown mode, a mode missing its input, and a
    // single-arm mode contradicting the other arm's input.
    for body in [
        json!({}),
        json!({"query": "east", "mode": "semantic"}),
        json!({"query": "east", "mode": "vector"}),
        json!({"vector": vector, "mode": "text"}),
        json!({"query": "east", "vector": vector, "mode": "text"}),
        json!({"query": "east", "vector": vector, "mode": "vector"}),
    ] {
        let (status, _) = search(&app, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn hybrid_search_fuses_text_and_vector_rankings() {
    let (app, _dir) = test_app().await;
    push_schema(&app, vector_schema_body()).await;

    // Text ranks alpha > beta > gamma by term frequency; the vector arm ranks
    // beta > gamma > alpha by angle to [1, 0, 0].
    for (slug, hits, embedding) in [
        ("alpha", 3, json!([0.0, 1.0, 0.0])),
        ("beta", 2, json!([1.0, 0.0, 0.0])),
        ("gamma", 1, json!([0.9, 0.1, 0.0])),
    ] {
        create_post(&app, ranked_post(slug, "published", hits, embedding)).await;
    }

    assert_eq!(
        search_slugs(&app, json!({"query": "rust"})).await,
        vec!["alpha", "beta", "gamma"]
    );
    assert_eq!(
        search_slugs(&app, json!({"vector": [1.0, 0.0, 0.0]})).await,
        vec!["beta", "gamma", "alpha"]
    );

    // RRF: beta is near the top of both arms and wins even though it leads
    // neither; gamma never places first and falls to last.
    let query = json!({"query": "rust", "vector": [1.0, 0.0, 0.0], "mode": "hybrid"});
    let hits = search_hits(&app, query.clone()).await;
    assert_eq!(
        hits.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(),
        vec!["beta", "alpha", "gamma"]
    );

    // Scores are rank sums over two arms at k=60, not BM25 or cosine values.
    assert!(
        hits.iter()
            .all(|(_, s)| *s > 0.0 && *s <= 2.0 / 61.0 + 1e-9),
        "hits: {:?}",
        hits
    );
    assert!(
        hits.windows(2).all(|w| w[0].1 >= w[1].1),
        "hits: {:?}",
        hits
    );

    // Sending both inputs infers hybrid without naming the mode.
    assert_eq!(
        search_slugs(&app, json!({"query": "rust", "vector": [1.0, 0.0, 0.0]})).await,
        vec!["beta", "alpha", "gamma"]
    );

    // A document only one arm can see collects that arm's contribution alone:
    // "textonly" now leads the text arm outright and still places last, since
    // one first place is worth less than placing respectably in both arms.
    create_post(&app, ranked_post("textonly", "published", 5, json!(null))).await;
    assert_eq!(
        search_slugs(&app, json!({"query": "rust"})).await[0],
        "textonly"
    );
    assert_eq!(
        search_slugs(&app, query).await,
        vec!["beta", "alpha", "gamma", "textonly"]
    );
}

#[tokio::test]
async fn hybrid_needs_both_index_kinds_and_both_inputs() {
    let (app, _dir) = test_app().await;
    let vector = json!([1.0, 0.0, 0.0]);
    let hybrid = json!({"query": "rust", "vector": vector, "mode": "hybrid"});

    // Only a search index declared.
    push_schema(&app, search_schema_body()).await;
    let (status, err) = search(&app, hybrid.clone()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        error_message(&err).contains("no vector index"),
        "error: {}",
        err["error"]
    );

    // Only a vector index declared.
    let mut schema = vector_schema_body();
    schema["collections"]["posts"]["searchIndexes"] = json!([]);
    push_schema(&app, schema).await;
    let (status, err) = search(&app, hybrid).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        error_message(&err).contains("no search index"),
        "error: {}",
        err["error"]
    );

    // Hybrid named explicitly still needs both inputs.
    push_schema(&app, vector_schema_body()).await;
    for (body, missing) in [
        (json!({"query": "rust", "mode": "hybrid"}), "vector"),
        (json!({"vector": vector, "mode": "hybrid"}), "query"),
    ] {
        let (status, err) = search(&app, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let message = error_message(&err);
        assert!(message.contains("hybrid"), "error: {}", message);
        assert!(message.contains(missing), "error: {}", message);
    }
}

#[tokio::test]
async fn hybrid_selects_an_index_per_arm() {
    let (app, _dir) = test_app().await;
    push_schema(&app, vector_schema_body()).await;
    create_post(
        &app,
        ranked_post("alpha", "published", 3, json!([1.0, 0.0, 0.0])),
    )
    .await;

    let vector = json!([1.0, 0.0, 0.0]);

    // One `index` cannot name both arms.
    let (status, err) = search(
        &app,
        json!({"query": "rust", "vector": vector, "index": "search"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        error_message(&err).contains("ambiguous"),
        "error: {}",
        err["error"]
    );

    // Naming each arm explicitly resolves the same indexes as the defaults.
    assert_eq!(
        search_slugs(
            &app,
            json!({
                "query": "rust",
                "vector": vector,
                "textIndex": "search",
                "vectorIndex": "by_embedding"
            })
        )
        .await,
        vec!["alpha"]
    );

    // Unknown names fail per arm.
    for (body, expected) in [
        (
            json!({"query": "rust", "vector": vector, "textIndex": "nope"}),
            "unknown search index",
        ),
        (
            json!({"query": "rust", "vector": vector, "vectorIndex": "nope"}),
            "unknown vector index",
        ),
    ] {
        let (status, err) = search(&app, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            error_message(&err).contains(expected),
            "error: {}",
            err["error"]
        );
    }

    // The per-arm names are hybrid-only; single-arm searches use `index`.
    for body in [
        json!({"query": "rust", "textIndex": "search"}),
        json!({"vector": vector, "vectorIndex": "by_embedding"}),
    ] {
        let (status, err) = search(&app, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            err["error"]
                .as_str()
                .unwrap()
                .contains("only applies to hybrid"),
            "error: {}",
            err["error"]
        );
    }
}

#[tokio::test]
async fn filter_narrows_every_mode() {
    let (app, _dir) = test_app().await;
    push_schema(&app, vector_schema_body()).await;

    // Drafts outrank the published posts in both arms, so an unfiltered
    // search leads with them and the filter has something to remove.
    for (slug, status, hits, embedding) in [
        ("draft-a", "draft", 5, json!([1.0, 0.0, 0.0])),
        ("draft-b", "draft", 4, json!([0.9, 0.1, 0.0])),
        ("live-a", "published", 3, json!([0.8, 0.2, 0.0])),
        ("live-b", "published", 2, json!([0.7, 0.3, 0.0])),
    ] {
        create_post(&app, ranked_post(slug, status, hits, embedding)).await;
    }

    let filter = json!({"index": "by_status", "params": {"status": "published"}});
    let vector = json!([1.0, 0.0, 0.0]);

    for body in [
        json!({"query": "rust", "filter": filter}),
        json!({"vector": vector, "filter": filter}),
        json!({"query": "rust", "vector": vector, "filter": filter}),
    ] {
        assert_eq!(
            search_slugs(&app, body.clone()).await,
            vec!["live-a", "live-b"],
            "body: {}",
            body
        );
    }

    // Without the filter the drafts come back too.
    assert_eq!(
        search_slugs(&app, json!({"query": "rust"})).await,
        vec!["draft-a", "draft-b", "live-a", "live-b"]
    );

    // A filter matching nothing is an empty page, not an error.
    assert!(search_slugs(
        &app,
        json!({"query": "rust", "filter": {"index": "by_slug", "params": {"slug": "absent"}}})
    )
    .await
    .is_empty());
}

#[tokio::test]
async fn filter_overfetches_past_the_requested_page() {
    let (app, _dir) = test_app().await;
    push_schema(&app, vector_schema_body()).await;

    // The five strongest text matches are drafts, so a filtered page can only
    // be filled from candidates below the requested limit.
    for hits in (1..=10).rev() {
        let status = if hits > 5 { "draft" } else { "published" };
        create_post(
            &app,
            ranked_post(
                &format!("doc-{}", hits),
                status,
                hits,
                json!([1.0, 0.0, 0.0]),
            ),
        )
        .await;
    }

    assert_eq!(
        search_slugs(&app, json!({"query": "rust", "limit": 2})).await,
        vec!["doc-10", "doc-9"]
    );

    // limit 2 retrieves 8 candidates: five drafts, then the published ones.
    assert_eq!(
        search_slugs(
            &app,
            json!({
                "query": "rust",
                "limit": 2,
                "filter": {"index": "by_status", "params": {"status": "published"}}
            })
        )
        .await,
        vec!["doc-5", "doc-4"]
    );
}

#[tokio::test]
async fn filter_rejects_malformed_and_unusable_predicates() {
    let (app, _dir) = test_app().await;
    push_schema(&app, vector_schema_body()).await;
    create_post(
        &app,
        ranked_post("alpha", "published", 3, json!([1.0, 0.0, 0.0])),
    )
    .await;

    for (filter, expected) in [
        (
            json!({"index": "nope", "params": {"status": "published"}}),
            "unknown filter index",
        ),
        // Params must bind a prefix of the index fields, as `_query` requires.
        (
            json!({"index": "by_status", "params": {"publishedAt": 100}}),
            "params must include the first field",
        ),
        (json!({"index": "by_slug", "params": {}}), "params must"),
        (json!("by_status"), "filter must be an object"),
        (json!({"params": {"status": "published"}}), "\"index\""),
        (json!({"index": "by_status"}), "\"params\""),
        (
            json!({"index": "by_status", "params": "published"}),
            "\"params\"",
        ),
    ] {
        let (status, err) = search(&app, json!({"query": "rust", "filter": filter})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "filter: {}", filter);
        assert!(
            error_message(&err).contains(expected),
            "filter: {}, error: {}",
            filter,
            err["error"]
        );
    }
}

#[tokio::test]
async fn schema_rejects_unusable_vector_indexes() {
    let (app, _dir) = test_app().await;
    let path = format!("/v1/{}/_schema", PROJECT);

    let push = |schema: Value| {
        let app = app.clone();
        let path = path.clone();
        async move { send(&app, "PUT", &path, Some(KEY), Some(schema)).await.0 }
    };

    // Index over a field that is not a vector would silently index nothing.
    let mut schema = vector_schema_body();
    schema["collections"]["posts"]["vectorIndexes"][0]["field"] = json!("title");
    assert_eq!(push(schema).await, StatusCode::BAD_REQUEST);

    let mut schema = vector_schema_body();
    schema["collections"]["posts"]["vectorIndexes"][0]["field"] = json!("absent");
    assert_eq!(push(schema).await, StatusCode::BAD_REQUEST);

    // Index width must agree with the field's declared width.
    let mut schema = vector_schema_body();
    schema["collections"]["posts"]["vectorIndexes"][0]["dims"] = json!(4);
    assert_eq!(push(schema).await, StatusCode::BAD_REQUEST);

    // Guardrail on how much one document can cost the in-memory cache.
    let mut schema = vector_schema_body();
    schema["collections"]["posts"]["fields"]["embedding"] = json!({"type": "vector", "dims": 8192});
    schema["collections"]["posts"]["vectorIndexes"][0]["dims"] = json!(8192);
    assert_eq!(push(schema).await, StatusCode::BAD_REQUEST);

    for dims in [json!(0), json!("many")] {
        let mut schema = vector_schema_body();
        schema["collections"]["posts"]["fields"]["embedding"] =
            json!({"type": "vector", "dims": dims});
        assert_eq!(push(schema).await, StatusCode::BAD_REQUEST);
    }

    // Names share the keyspace rules of the other index kinds.
    let mut schema = vector_schema_body();
    schema["collections"]["posts"]["vectorIndexes"][0]["name"] = json!("by:embedding");
    assert_eq!(push(schema).await, StatusCode::BAD_REQUEST);

    let mut schema = vector_schema_body();
    schema["collections"]["posts"]["vectorIndexes"] = json!([
        {"name": "by_embedding", "field": "embedding", "metric": "cosine", "dims": 3},
        {"name": "by_embedding", "field": "embedding", "metric": "dot", "dims": 3}
    ]);
    assert_eq!(push(schema).await, StatusCode::BAD_REQUEST);

    // A required (non-optional) vector field is accepted, and metric defaults.
    let mut schema = vector_schema_body();
    schema["collections"]["posts"]["fields"]["embedding"] = json!({"type": "vector", "dims": 3});
    schema["collections"]["posts"]["vectorIndexes"] =
        json!([{"name": "by_embedding", "field": "embedding", "dims": 3}]);
    assert_eq!(push(schema).await, StatusCode::OK);
}

#[tokio::test]
async fn instance_keys_authenticate_and_scope() {
    let broker = KeyBroker::from_hex(INSTANCE, SECRET).unwrap();
    let (app, _dir) = test_app_with_broker(Some(broker.clone())).await;
    let base = format!("/v1/{}/posts", PROJECT);

    // A key minted for this project works end to end.
    let key = broker.issue(PROJECT).unwrap();
    let (status, created) = send(
        &app,
        "POST",
        &base,
        Some(&key),
        Some(json!({"title": "minted"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["_id"].as_str().unwrap().to_string();

    let (status, _) = send(&app, "GET", &format!("{}/{}", base, id), Some(&key), None).await;
    assert_eq!(status, StatusCode::OK);

    // Scoping is enforced: this key reaches nothing else.
    let (status, _) = send(&app, "GET", "/v1/otherproj/posts", Some(&key), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A wildcard key reaches any project.
    let wildcard = broker.issue("*").unwrap();
    let (status, _) = send(&app, "GET", "/v1/otherproj/posts", Some(&wildcard), None).await;
    assert_eq!(status, StatusCode::OK);

    // Static API_KEYS still work alongside minted keys.
    let (status, _) = send(&app, "GET", &base, Some(KEY), None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn forged_and_foreign_instance_keys_are_rejected() {
    let broker = KeyBroker::from_hex(INSTANCE, SECRET).unwrap();
    let (app, _dir) = test_app_with_broker(Some(broker.clone())).await;
    let base = format!("/v1/{}/posts", PROJECT);

    // Minted under a different secret.
    let other_secret = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100";
    let foreign = KeyBroker::from_hex(INSTANCE, other_secret).unwrap();
    let (status, _) = send(
        &app,
        "GET",
        &base,
        Some(&foreign.issue(PROJECT).unwrap()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Minted for a different instance name.
    let elsewhere = KeyBroker::from_hex("other-instance", SECRET).unwrap();
    let (status, _) = send(
        &app,
        "GET",
        &base,
        Some(&elsewhere.issue(PROJECT).unwrap()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Tampered payload: last character of a valid key flipped.
    let key = broker.issue(PROJECT).unwrap();
    let mut chars: Vec<char> = key.chars().collect();
    let last = chars.len() - 1;
    chars[last] = if chars[last] == 'A' { 'B' } else { 'A' };
    let tampered: String = chars.into_iter().collect();
    let (status, _) = send(&app, "GET", &base, Some(&tampered), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A key that would be valid is still rejected when the broker is disabled.
    let (app_no_broker, _dir) = test_app().await;
    let (status, _) = send(
        &app_no_broker,
        "GET",
        &base,
        Some(&broker.issue(PROJECT).unwrap()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Schema whose `embedding` is server-filled from the text fields. Declared
/// required (not optional) so the tests also cover a document that only
/// validates because the server supplies the field.
fn embed_schema_body() -> Value {
    let mut schema = search_schema_body();
    schema["collections"]["posts"]["fields"]["embedding"] = json!({
        "type": "vector",
        "dims": 4,
        "from": ["title", "body"],
        "model": "test-model"
    });
    schema["collections"]["posts"]["vectorIndexes"] = json!([
        {"name": "by_embedding", "field": "embedding", "metric": "cosine", "dims": 4}
    ]);
    schema
}

fn mock_app_provider() -> Option<Arc<dyn EmbeddingProvider>> {
    Some(Arc::new(MockProvider::new(4)))
}

/// The stored embedding of a document, or `None` when it has none.
async fn embedding_of(app: &Router, id: &str) -> Option<Vec<f64>> {
    let (status, doc) = send(
        app,
        "GET",
        &format!("/v1/{}/posts/{}", PROJECT, id),
        Some(KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    doc.get("embedding")
        .and_then(Value::as_array)
        .map(|components| {
            components
                .iter()
                .map(|c| c.as_f64().expect("numeric component"))
                .collect()
        })
}

#[tokio::test]
async fn embed_on_write_fills_a_missing_vector() {
    let (app, _dir) = test_app_with_embedder(mock_app_provider()).await;
    push_schema(&app, embed_schema_body()).await;

    let id = create_post(
        &app,
        searchable_post("alpha", "Rust storage", "A fast engine."),
    )
    .await;

    let vector = embedding_of(&app, &id).await.expect("server-filled vector");
    assert_eq!(vector.len(), 4);
    assert!(vector.iter().all(|c| c.is_finite()), "vector: {:?}", vector);

    // The document is vector-indexed: searching by its own embedding finds it.
    assert_eq!(
        search_slugs(&app, json!({"vector": vector})).await,
        vec!["alpha"]
    );

    // Same source text embeds identically, different text does not.
    let same = create_post(
        &app,
        searchable_post("beta", "Rust storage", "A fast engine."),
    )
    .await;
    assert_eq!(embedding_of(&app, &same).await.as_ref(), Some(&vector));

    let other = create_post(
        &app,
        searchable_post("gamma", "Baking bread", "Sourdough starter."),
    )
    .await;
    assert_ne!(embedding_of(&app, &other).await.as_ref(), Some(&vector));
}

#[tokio::test]
async fn a_client_supplied_vector_is_never_overwritten() {
    let (app, _dir) = test_app_with_embedder(mock_app_provider()).await;
    push_schema(&app, embed_schema_body()).await;

    let mut doc = searchable_post("alpha", "Rust storage", "A fast engine.");
    doc["embedding"] = json!([0.1, 0.2, 0.3, 0.4]);
    let id = create_post(&app, doc).await;

    assert_eq!(
        embedding_of(&app, &id).await,
        Some(vec![0.1, 0.2, 0.3, 0.4]),
        "the client's vector must survive verbatim"
    );
}

#[tokio::test]
async fn replace_and_patch_re_embed_matrix() {
    let (app, _dir) = test_app_with_embedder(mock_app_provider()).await;
    let base = format!("/v1/{}/posts", PROJECT);
    push_schema(&app, embed_schema_body()).await;

    let id = create_post(
        &app,
        searchable_post("alpha", "Rust storage", "A fast engine."),
    )
    .await;
    let original = embedding_of(&app, &id).await.unwrap();

    // Replace with new text and no vector: re-embedded.
    let (status, _) = send(
        &app,
        "PUT",
        &format!("{}/{}", base, id),
        Some(KEY),
        Some(searchable_post(
            "alpha",
            "Baking bread",
            "Sourdough starter.",
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let replaced = embedding_of(&app, &id).await.unwrap();
    assert_ne!(replaced, original);

    // Replace supplying a vector: stored verbatim.
    let mut doc = searchable_post("alpha", "Rust storage", "A fast engine.");
    doc["embedding"] = json!([1.0, 0.0, 0.0, 0.0]);
    let (status, _) = send(
        &app,
        "PUT",
        &format!("{}/{}", base, id),
        Some(KEY),
        Some(doc),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        embedding_of(&app, &id).await,
        Some(vec![1.0, 0.0, 0.0, 0.0])
    );

    let patch = |body: Value| {
        let app = app.clone();
        let path = format!("{}/{}", base, id);
        async move {
            let (status, _) = send(&app, "PATCH", &path, Some(KEY), Some(body)).await;
            assert_eq!(status, StatusCode::OK);
        }
    };

    // Patching a field the embedding does not read leaves it alone.
    patch(json!({"slug": "renamed"})).await;
    assert_eq!(
        embedding_of(&app, &id).await,
        Some(vec![1.0, 0.0, 0.0, 0.0]),
        "an unrelated patch must not call the provider"
    );

    // Patching a source field re-embeds.
    patch(json!({"title": "Rewritten title"})).await;
    let after_title = embedding_of(&app, &id).await.unwrap();
    assert_ne!(after_title, vec![1.0, 0.0, 0.0, 0.0]);

    // Writing a source field its existing value is not a change.
    patch(json!({"title": "Rewritten title"})).await;
    assert_eq!(embedding_of(&app, &id).await, Some(after_title.clone()));

    // An explicit vector wins even when the same patch changes a source.
    patch(json!({"title": "Changed again", "embedding": [0.0, 1.0, 0.0, 0.0]})).await;
    assert_eq!(
        embedding_of(&app, &id).await,
        Some(vec![0.0, 1.0, 0.0, 0.0])
    );
}

#[tokio::test]
async fn provider_failure_is_502_and_persists_nothing() {
    let provider: Arc<dyn EmbeddingProvider> = Arc::new(MockProvider::failing("provider is down"));
    let (app, _dir) = test_app_with_embedder(Some(provider)).await;
    let base = format!("/v1/{}/posts", PROJECT);
    push_schema(&app, embed_schema_body()).await;

    // A write that needs an embedding fails, and nothing is written.
    let (status, err) = send(
        &app,
        "POST",
        &base,
        Some(KEY),
        Some(searchable_post("alpha", "Rust storage", "A fast engine.")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(error_code(&err), "embedding_provider_error");
    assert!(
        error_message(&err).contains("embedding provider request failed"),
        "error: {}",
        err["error"]
    );
    assert!(!error_message(&err).contains("provider is down"));

    let (status, listed) = send(&app, "GET", &base, Some(KEY), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        listed["data"].as_array().unwrap().is_empty(),
        "the failed write must not have persisted a document"
    );

    // A document carrying its own vector needs no provider, so it still writes.
    let mut doc = searchable_post("beta", "Rust storage", "A fast engine.");
    doc["embedding"] = json!([1.0, 0.0, 0.0, 0.0]);
    let id = create_post(&app, doc).await;

    // Patching a source field now needs the provider: the document is left as
    // it was rather than saved without its re-embedding.
    let (status, _) = send(
        &app,
        "PATCH",
        &format!("{}/{}", base, id),
        Some(KEY),
        Some(json!({"title": "Rewritten"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);

    let (_, doc) = send(&app, "GET", &format!("{}/{}", base, id), Some(KEY), None).await;
    assert_eq!(doc["title"], "Rust storage");
    assert_eq!(
        embedding_of(&app, &id).await,
        Some(vec![1.0, 0.0, 0.0, 0.0])
    );
}

#[tokio::test]
async fn without_a_provider_documents_write_unembedded() {
    let (app, _dir) = test_app().await;
    push_schema(&app, embed_schema_body()).await;

    // The vector field is declared required, but it is server-filled: with no
    // provider the document is accepted and simply left out of the index.
    let id = create_post(
        &app,
        searchable_post("alpha", "Rust storage", "A fast engine."),
    )
    .await;
    assert_eq!(embedding_of(&app, &id).await, None);
    assert!(search_slugs(&app, json!({"vector": [1.0, 0.0, 0.0, 0.0]}))
        .await
        .is_empty());

    // Text search is unaffected.
    assert_eq!(
        search_slugs(&app, json!({"query": "rust"})).await,
        vec!["alpha"]
    );

    // A client-supplied vector is still stored and indexed.
    let mut doc = searchable_post("beta", "Baking bread", "Sourdough starter.");
    doc["embedding"] = json!([1.0, 0.0, 0.0, 0.0]);
    create_post(&app, doc).await;
    assert_eq!(
        search_slugs(&app, json!({"vector": [1.0, 0.0, 0.0, 0.0]})).await,
        vec!["beta"]
    );
}

#[tokio::test]
async fn schema_rejects_unusable_embed_sources() {
    let (app, _dir) = test_app_with_embedder(mock_app_provider()).await;
    let path = format!("/v1/{}/_schema", PROJECT);

    let push = |schema: Value| {
        let app = app.clone();
        let path = path.clone();
        async move { send(&app, "PUT", &path, Some(KEY), Some(schema)).await }
    };

    let with_embedding = |embedding: Value| {
        let mut schema = embed_schema_body();
        schema["collections"]["posts"]["fields"]["embedding"] = embedding;
        schema
    };

    for (embedding, expected) in [
        (
            json!({"type": "vector", "dims": 4, "from": ["absent"], "model": "m"}),
            "unknown field",
        ),
        // publishedAt is a number: there is no text to embed.
        (
            json!({"type": "vector", "dims": 4, "from": ["publishedAt"], "model": "m"}),
            "no text to embed",
        ),
        (
            json!({"type": "vector", "dims": 4, "from": [], "model": "m"}),
            "at least one field",
        ),
        (
            json!({"type": "vector", "dims": 4, "from": ["title"]}),
            "\"model\"",
        ),
        (
            json!({"type": "vector", "dims": 4, "from": ["title"], "model": "  "}),
            "must not be empty",
        ),
        (
            json!({"type": "vector", "dims": 4, "from": ["embedding"], "model": "m"}),
            "cannot embed itself",
        ),
        (
            json!({"type": "vector", "dims": 4, "from": "title", "model": "m"}),
            "must be an array",
        ),
    ] {
        let (status, err) = push(with_embedding(embedding.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "embedding: {}", embedding);
        assert!(
            error_message(&err).contains(expected),
            "embedding: {}, error: {}",
            embedding,
            err["error"]
        );
    }

    // `from` on something that is not a vector.
    let mut schema = embed_schema_body();
    schema["collections"]["posts"]["fields"]["slug"] =
        json!({"type": "string", "from": ["title"], "model": "m"});
    let (status, err) = push(schema).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        err["error"]
            .as_str()
            .unwrap()
            .contains("only applies to a vector field"),
        "error: {}",
        err["error"]
    );

    // Arrays of strings and optional text are embeddable sources.
    let (status, _) = push(with_embedding(json!({
        "type": "optional",
        "inner": {"type": "vector", "dims": 4, "from": ["title", "body", "tags"], "model": "m"}
    })))
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn emptying_the_sources_removes_the_stale_vector() {
    let (app, _dir) = test_app_with_embedder(mock_app_provider()).await;
    let base = format!("/v1/{}/posts", PROJECT);
    push_schema(&app, embed_schema_body()).await;

    let id = create_post(
        &app,
        searchable_post("alpha", "Rust storage", "A fast engine."),
    )
    .await;
    let vector = embedding_of(&app, &id).await.expect("server-filled vector");
    assert_eq!(
        search_slugs(&app, json!({"vector": vector})).await,
        vec!["alpha"]
    );

    // Blank out every source. There is nothing left to embed, and the vector
    // already stored describes text the document no longer has.
    let (status, _) = send(
        &app,
        "PATCH",
        &format!("{}/{}", base, id),
        Some(KEY),
        Some(json!({"title": "", "body": ""})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(
        embedding_of(&app, &id).await,
        None,
        "the vector for the old text must not survive"
    );
    assert!(
        search_slugs(&app, json!({"vector": [1.0, 0.0, 0.0, 0.0]}))
            .await
            .is_empty(),
        "the document must leave the vector index"
    );

    // Giving it text again re-embeds it.
    let (status, _) = send(
        &app,
        "PATCH",
        &format!("{}/{}", base, id),
        Some(KEY),
        Some(json!({"title": "Rust storage"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(embedding_of(&app, &id).await.is_some());
}

// ---- end-user auth + rules ----
//
// Tokens are minted here with a throwaway RSA key and the matching JWKS is
// preloaded into the cache, so nothing in these tests touches the network.

const ISSUER: &str = "https://idp.test";
const KID: &str = "integration-key";

/// Body of a test-only RSA key. Build the PEM marker at runtime so secret
/// scanners do not mistake this committed public fixture for a live key.
const PRIVATE_PEM_BODY: &str = "MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQC2D+cQNSWgjtOD
CHxIuHLW3t36mrf5bhP5c6LMPLOpKPVWj2hmye+nZjCTrfphQyCjQtJSsYwbygr8
ombHTIpiDUkaZ8f7Lt8lfngWb8g12eBalAr/eD29R8e6GZNLgpuaz0ds0/Fqi32J
ppM/5OoQQD0hWYKW17A0LwaOP0Nq7CyTKhJKRaqhYsjc5r7iWX9Kw3F5xoWyCR+T
sx5W6hnwy0hBUvRYfe8M4E4FfXWfvZg866m3XS4Ie9swesOJdP/KhHV6MCVs03gp
VHRvX4tCPuPMsYa5YFiRzmqU0yg74fBFI83zaqVx7j0ruv1RB60S5i0nvvEfWZKi
iLOwN6mZAgMBAAECggEAJjSIm9NXUc3fD2aCtDz2pmYO5YW9uStTKDwOq/bKCePF
NjSyZy2Vq8aLR5ZRDkOIsBH38m/9M6CutQy8bjK+8GwSzOZ95hVUMPlM6IJtHHXb
+Y4LD8izBgsv27r9uLEQB8jbF3iTdvUOM0pgKJ6YSrA8U8kFmTNqb8z1fnjCpEdP
W70YYXGlnzwGNFoUESbmcIh4+POkHOCQ7c2cvrpC+S0rXsKQVFmK4pK5n4ngkdwg
AVa4TARGruEW7oibSTRnn/gervjGnBNmbjdiSnKU+cvYKh+0jCaN1UJqXKAxwaHF
jMTKTMnxMZFSFN3L2rCcXt5C01Lt9qPM6o+DlNkeTQKBgQD5BMC4mqVuVBYHfOoJ
tfB53/mk/RByRj6/nuoGx93ydf2pltNX2dCkNVsmP7XXxpGdL19xLhwrJWFMzIdg
T789eh8AMsl51YG6HlkAU15KIfhIAzDAvb0syWHEiGdXLTE9PH9sFBPJ8DAdfwh4
Tl+4Yto/zHon8qXyvq9i3Sb39QKBgQC7KpekbB/t9LX9Gz0Z7eO4qDHBemVyNc9W
1TgivC7JjpQSg7BFwfmokEMOcVSiEJL46ubsAynQXobHj/y3gFIFcw55fR0kQxfl
1qRPUP97mKsNimlnhZxLFPn1QcYYI9JfygV6jI+Za68sbTqddfIEV8PbkZ5AOgxH
j/bRJoT4lQKBgQCGUHb224sBgF9FeK3vwO/NfO51fH4jdRohV0DZmXJwdg31LEIg
b37nI1RfxBt8IEGoa8XqETnmV8osl2EppLn9GeKgw8QCcBQB5J6S22TPTZVSmk3w
mCbygki2rfA3iEu3wOrly8qEsIXzUvKpmXRtyvv3T35QD8RMs2d8RtbfBQKBgEb5
e8+qAOGnbmuwrJbskvIvNc78rwOETD/NUyA45DUikBwFPA7348h8DDGp4EIkrtcd
nLva5zxQ3CNJArhDPNc8Ljz7qNVba/CIWH6LZJZl6leUKSxMilwedDsA2jHFQ713
SmSScNHo9+CM+zFCzKfA8FCPA8evO4DXouzlAn+RAoGBAIG5f7P8JntCiSyHPeD4
jaIfJlMpAk4y/isBqGI6VQsWun3gAp3HZJ36XQy3wx6K3cxFgQhIeK7L11n2Qmba
8kaG8WXheNN6Mw3vBgXHgjlTDueQY4km/smo9+g1obbngV5L2ybad3c7qv9CglL2
1rhPYWWa1AVDkyIHH6i3JoeX";

fn test_private_pem() -> String {
    format!(
        "-----BEGIN {}-----\n{}\n-----END {}-----",
        "PRIVATE KEY", PRIVATE_PEM_BODY, "PRIVATE KEY"
    )
}

const MODULUS: &str = "tg_nEDUloI7Tgwh8SLhy1t7d-pq3-W4T-XOizDyzqSj1Vo9oZsnvp2Ywk636YUMgo0LSUrGMG8oK_KJmx0yKYg1JGmfH-y7fJX54Fm_INdngWpQK_3g9vUfHuhmTS4Kbms9HbNPxaot9iaaTP-TqEEA9IVmCltewNC8Gjj9DauwskyoSSkWqoWLI3Oa-4ll_SsNxecaFsgkfk7MeVuoZ8MtIQVL0WH3vDOBOBX11n72YPOupt10uCHvbMHrDiXT_yoR1ejAlbNN4KVR0b1-LQj7jzLGGuWBYkc5qlNMoO-HwRSPN82qlce49K7r9UQetEuYtJ77xH1mSooizsDepmQ";

/// Router whose JWKS cache already holds the test signing key.
async fn test_app_with_identity() -> (Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        host: "127.0.0.1".to_string(),
        port: 0,
        storage: StorageBackend::Local {
            path: dir.path().to_str().unwrap().to_string(),
        },
        db_path: "test".to_string(),
        api_keys: HashMap::from([
            (KEY.to_string(), PROJECT.to_string()),
            ("admin-key".to_string(), "*".to_string()),
        ]),
        keybroker: None,
        embeddings: None,
    };
    let db = Db::open(&config).await.unwrap();
    let jwks = JwksCache::new(HttpJwksFetcher::new());
    jwks.preload(
        ISSUER,
        serde_json::from_value(json!({
            "keys": [{
                "kty": "RSA", "alg": "RS256", "use": "sig",
                "kid": KID, "n": MODULUS, "e": "AQAB"
            }]
        }))
        .unwrap(),
        brickbed_server::jwt::now_ms(),
    )
    .await;

    let state = Arc::new(AppState {
        db,
        api_keys: config.api_keys.clone(),
        keybroker: None,
        jwks,
    });
    (build_router(state), dir)
}

fn token_for(subject: &str) -> String {
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(KID.to_string());
    let claims = json!({
        "iss": ISSUER,
        "sub": subject,
        "email": format!("{}@example.com", subject),
        "exp": jsonwebtoken::get_current_timestamp() + 3600,
    });
    jsonwebtoken::encode(
        &header,
        &claims,
        &jsonwebtoken::EncodingKey::from_rsa_pem(test_private_pem().as_bytes()).unwrap(),
    )
    .unwrap()
}

/// Schema declaring the test provider plus `rules` on `posts`.
fn ruled_schema(rules: Value) -> Value {
    let mut schema = search_schema_body();
    schema["auth"] = json!({"providers": [{"issuer": ISSUER, "algorithms": ["RS256"]}]});
    schema["collections"]["posts"]["fields"]["authorId"] =
        json!({"type": "optional", "inner": {"type": "string"}});
    schema["collections"]["posts"]["rules"] = rules;
    schema
}

/// Schema pushes that set `auth` need the wildcard key.
async fn push_ruled_schema(app: &Router, rules: Value) {
    let (status, body) = send(
        app,
        "PUT",
        &format!("/v1/{}/_schema", PROJECT),
        Some("admin-key"),
        Some(ruled_schema(rules)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "schema push failed: {}", body);
}

fn owned_post(slug: &str, owner: &str) -> Value {
    let mut doc = searchable_post(slug, &format!("Post {}", slug), "Body text about rust.");
    doc["authorId"] = json!(owner);
    doc
}

#[tokio::test]
async fn every_v1_route_denies_anonymous_by_default() {
    let (app, _dir) = test_app_with_identity().await;
    // A schema with no rules at all: the pre-rules status quo.
    push_schema(&app, schema_body()).await;

    for (method, template) in brickbed_server::V1_ROUTES {
        let path = template
            .replace(":project", PROJECT)
            .replace(":collection", "posts")
            .replace(":id", "01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let body = match *method {
            "POST" | "PUT" | "PATCH" => Some(json!({})),
            _ => None,
        };
        let (status, _) = send(&app, method, &path, None, body).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{} {} let an anonymous caller through",
            method,
            path
        );
    }
}

#[tokio::test]
async fn route_table_matches_the_router() {
    // The coverage test above is only as good as the table it walks, so the
    // table has to track the router. Both live in lib.rs; compare them here.
    let source = include_str!("../src/lib.rs");
    let registered = source.matches(".route(").count() - 2; // minus /health and /ready
    assert_eq!(
        registered,
        brickbed_server::V1_ROUTES.len(),
        "lib.rs registers {} /v1 routes but V1_ROUTES lists {}",
        registered,
        brickbed_server::V1_ROUTES.len()
    );

    // Every `/v1` path literal in lib.rs must appear in the table.
    for line in source.lines() {
        if let Some(start) = line.find("\"/v1") {
            let literal = &line[start + 1..];
            let literal = &literal[..literal.find('"').unwrap()];
            assert!(
                brickbed_server::V1_ROUTES
                    .iter()
                    .any(|(_, p)| *p == literal),
                "path {:?} in lib.rs is missing from V1_ROUTES",
                literal
            );
        }
    }
}

#[tokio::test]
async fn public_rule_admits_anonymous_readers_only() {
    let (app, _dir) = test_app_with_identity().await;
    push_ruled_schema(&app, json!({"read": "public"})).await;
    let base = format!("/v1/{}/posts", PROJECT);

    let (status, created) = send(&app, "POST", &base, Some(KEY), Some(owned_post("a", "u1"))).await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["_id"].as_str().unwrap().to_string();

    // No credential at all can read.
    for path in [base.clone(), format!("{}/{}", base, id)] {
        let (status, _) = send(&app, "GET", &path, None, None).await;
        assert_eq!(status, StatusCode::OK, "path: {}", path);
    }
    let (status, _) = send(
        &app,
        "POST",
        &format!("{}/_search", base),
        None,
        Some(json!({"query": "rust"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // But a read rule grants no writes.
    for (method, path, body) in [
        ("POST", base.clone(), Some(owned_post("b", "u1"))),
        (
            "PATCH",
            format!("{}/{}", base, id),
            Some(json!({"title": "x"})),
        ),
        ("DELETE", format!("{}/{}", base, id), None),
    ] {
        let (status, _) = send(&app, method, &path, None, body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{} {}", method, path);
    }
}

#[tokio::test]
async fn authenticated_rule_requires_a_valid_token() {
    let (app, _dir) = test_app_with_identity().await;
    push_ruled_schema(&app, json!({"read": "authenticated"})).await;
    let base = format!("/v1/{}/posts", PROJECT);
    send(&app, "POST", &base, Some(KEY), Some(owned_post("a", "u1"))).await;

    // Anonymous is refused, a real token works.
    let (status, _) = send(&app, "GET", &base, None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, listed) = send(&app, "GET", &base, Some(&token_for("u1")), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);

    // A token from an issuer the schema does not name is refused.
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(KID.to_string());
    let foreign = jsonwebtoken::encode(
        &header,
        &json!({
            "iss": "https://attacker.test", "sub": "u1",
            "exp": jsonwebtoken::get_current_timestamp() + 3600
        }),
        &jsonwebtoken::EncodingKey::from_rsa_pem(test_private_pem().as_bytes()).unwrap(),
    )
    .unwrap();
    let (status, _) = send(&app, "GET", &base, Some(&foreign), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Garbage bearer tokens stay 401 rather than degrading to anonymous.
    let (status, _) = send(&app, "GET", &base, Some("not-a-key"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn owner_rule_scopes_reads_and_writes() {
    let (app, _dir) = test_app_with_identity().await;
    push_ruled_schema(
        &app,
        json!({"read": {"owner": "authorId"}, "write": {"owner": "authorId"}}),
    )
    .await;
    let base = format!("/v1/{}/posts", PROJECT);

    // Seeded with an API key, which bypasses rules.
    let (_, mine) = send(
        &app,
        "POST",
        &base,
        Some(KEY),
        Some(owned_post("mine", "u1")),
    )
    .await;
    let (_, theirs) = send(
        &app,
        "POST",
        &base,
        Some(KEY),
        Some(owned_post("theirs", "u2")),
    )
    .await;
    let mine_id = mine["_id"].as_str().unwrap().to_string();
    let theirs_id = theirs["_id"].as_str().unwrap().to_string();

    let u1 = token_for("u1");

    // Reads: own document yes, someone else's no.
    let (status, _) = send(
        &app,
        "GET",
        &format!("{}/{}", base, mine_id),
        Some(&u1),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(
        &app,
        "GET",
        &format!("{}/{}", base, theirs_id),
        Some(&u1),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Listing and search only return owned documents.
    let (_, listed) = send(&app, "GET", &base, Some(&u1), None).await;
    let slugs: Vec<&str> = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["slug"].as_str().unwrap())
        .collect();
    assert_eq!(slugs, vec!["mine"]);

    let (_, found) = send(
        &app,
        "POST",
        &format!("{}/_search", base),
        Some(&u1),
        Some(json!({"query": "rust"})),
    )
    .await;
    let slugs: Vec<&str> = found["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["slug"].as_str().unwrap())
        .collect();
    assert_eq!(slugs, vec!["mine"], "search leaked another user's document");

    // Writes: own document yes, someone else's no.
    let (status, _) = send(
        &app,
        "PATCH",
        &format!("{}/{}", base, mine_id),
        Some(&u1),
        Some(json!({"title": "renamed"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(
        &app,
        "PATCH",
        &format!("{}/{}", base, theirs_id),
        Some(&u1),
        Some(json!({"title": "stolen"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Creating a document owned by someone else is refused, not rewritten.
    let (status, _) = send(&app, "POST", &base, Some(&u1), Some(owned_post("c", "u2"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(&app, "POST", &base, Some(&u1), Some(owned_post("c", "u1"))).await;
    assert_eq!(status, StatusCode::CREATED);

    // Deleting: only your own.
    let (status, _) = send(
        &app,
        "DELETE",
        &format!("{}/{}", base, theirs_id),
        Some(&u1),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(
        &app,
        "DELETE",
        &format!("{}/{}", base, mine_id),
        Some(&u1),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn owner_updates_are_checked_on_both_sides() {
    let (app, _dir) = test_app_with_identity().await;
    push_ruled_schema(
        &app,
        json!({"write": {"owner": "authorId"}, "read": "public"}),
    )
    .await;
    let base = format!("/v1/{}/posts", PROJECT);

    let (_, mine) = send(
        &app,
        "POST",
        &base,
        Some(KEY),
        Some(owned_post("mine", "u1")),
    )
    .await;
    let (_, theirs) = send(
        &app,
        "POST",
        &base,
        Some(KEY),
        Some(owned_post("theirs", "u2")),
    )
    .await;
    let mine_id = mine["_id"].as_str().unwrap().to_string();
    let theirs_id = theirs["_id"].as_str().unwrap().to_string();
    let u1 = token_for("u1");

    // Giving your document away.
    let (status, _) = send(
        &app,
        "PATCH",
        &format!("{}/{}", base, mine_id),
        Some(&u1),
        Some(json!({"authorId": "u2"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "handed a document to u2");

    // Stealing one by claiming it in the update.
    let (status, _) = send(
        &app,
        "PATCH",
        &format!("{}/{}", base, theirs_id),
        Some(&u1),
        Some(json!({"authorId": "u1"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "stole u2's document");

    // Same rules through PUT, which replaces rather than merges.
    let (status, _) = send(
        &app,
        "PUT",
        &format!("{}/{}", base, mine_id),
        Some(&u1),
        Some(owned_post("mine", "u2")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A patch that never mentions the owner field inherits it and is fine.
    let (status, _) = send(
        &app,
        "PATCH",
        &format!("{}/{}", base, mine_id),
        Some(&u1),
        Some(json!({"title": "still mine"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn auth_config_changes_need_a_wildcard_key() {
    let (app, _dir) = test_app_with_identity().await;
    let path = format!("/v1/{}/_schema", PROJECT);

    // A project-scoped key may not introduce auth providers.
    let (status, _) = send(
        &app,
        "PUT",
        &path,
        Some(KEY),
        Some(ruled_schema(json!({"read": "public"}))),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // The wildcard key may.
    let (status, _) = send(
        &app,
        "PUT",
        &path,
        Some("admin-key"),
        Some(ruled_schema(json!({"read": "public"}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // With auth unchanged, a project key can still push ordinary edits.
    let (status, _) = send(
        &app,
        "PUT",
        &path,
        Some(KEY),
        Some(ruled_schema(json!({"read": "authenticated"}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Repointing the issuer needs the wildcard key again...
    let mut moved = ruled_schema(json!({"read": "public"}));
    moved["auth"]["providers"][0]["issuer"] = json!("https://attacker.test");
    let (status, _) = send(&app, "PUT", &path, Some(KEY), Some(moved)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // ...and so does removing auth entirely.
    let (status, _) = send(&app, "PUT", &path, Some(KEY), Some(schema_body())).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // End users never reach the schema at all.
    let (status, _) = send(&app, "GET", &path, Some(&token_for("u1")), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn schema_push_rejects_unusable_auth_config() {
    let (app, _dir) = test_app_with_identity().await;
    let path = format!("/v1/{}/_schema", PROJECT);

    let push = |schema: Value| {
        let app = app.clone();
        let path = path.clone();
        async move {
            send(&app, "PUT", &path, Some("admin-key"), Some(schema))
                .await
                .0
        }
    };

    // Symmetric algorithms would let anyone holding the public key mint tokens.
    let mut schema = ruled_schema(json!({"read": "authenticated"}));
    schema["auth"]["providers"][0]["algorithms"] = json!(["HS256"]);
    assert_eq!(push(schema).await, StatusCode::BAD_REQUEST);

    // Plain-http and non-URL issuers.
    for issuer in [json!("http://idp.test"), json!("idp.test")] {
        let mut schema = ruled_schema(json!({"read": "authenticated"}));
        schema["auth"]["providers"][0]["issuer"] = issuer;
        assert_eq!(push(schema).await, StatusCode::BAD_REQUEST);
    }

    // Identity rules with no provider to authenticate against.
    let mut schema = search_schema_body();
    schema["collections"]["posts"]["rules"] = json!({"read": "authenticated"});
    assert_eq!(push(schema).await, StatusCode::BAD_REQUEST);

    // But public rules need no providers.
    let mut schema = search_schema_body();
    schema["collections"]["posts"]["rules"] = json!({"read": "public"});
    assert_eq!(push(schema).await, StatusCode::OK);

    // A typo'd rule key must be a push error, never a silent default.
    let mut schema = ruled_schema(json!({"read": {"owner": "authorId", "matsh": "email"}}));
    schema["auth"] = json!({"providers": [{"issuer": ISSUER}]});
    assert_eq!(push(schema).await, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn malformed_bodies_come_back_as_json_errors() {
    let (app, _dir) = test_app_with_identity().await;
    push_schema(&app, schema_body()).await;
    let base = format!("/v1/{}/posts", PROJECT);

    // Every body-taking route, with a body that cannot deserialize.
    let cases = [
        (format!("{}/_query", base), json!({"index": 42})),
        (format!("{}/_search", base), json!({"limit": "ten"})),
        (base.clone(), json!("not an object")),
    ];
    for (path, body) in cases {
        let (status, err) = send(&app, "POST", &path, Some(KEY), Some(body)).await;
        assert!(
            status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
            "path {} returned {}",
            path,
            status
        );
        assert_eq!(
            error_code(&err),
            "invalid_request",
            "path {}: {}",
            path,
            err
        );
        assert!(
            err["requestId"].as_str().is_some(),
            "path {}: {}",
            path,
            err
        );
    }

    // Syntactically broken JSON, which axum rejects before deserializing.
    let req = Request::builder()
        .method("POST")
        .uri(format!("{}/_search", base))
        .header("Authorization", format!("Bearer {}", KEY))
        .header("Content-Type", "application/json")
        .body(Body::from("{not json"))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let parsed: Value = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        panic!(
            "non-JSON body {:?} ({})",
            String::from_utf8_lossy(&bytes),
            e
        )
    });
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&parsed), "invalid_request", "body: {}", parsed);
    assert!(parsed["requestId"].as_str().is_some(), "body: {}", parsed);
}

#[tokio::test]
async fn errors_have_a_safe_request_id_and_v1_contract() {
    let (app, _dir) = test_app().await;
    let supplied = "client-request-123";
    let req = Request::builder()
        .method("GET")
        .uri("/v1/testproj/posts/missing")
        .header("Authorization", format!("Bearer {}", KEY))
        .header("X-Request-Id", supplied)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert_eq!(res.headers()["x-request-id"], supplied);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(error_code(&body), "not_found");
    assert_eq!(body["requestId"], supplied);
    assert!(body["error"].get("details").is_none());

    // A value containing unsafe characters is not reflected into the response
    // or logs. The generated ULID is bounded and has no caller-controlled text.
    let req = Request::builder()
        .method("GET")
        .uri("/does-not-exist")
        .header("Authorization", format!("Bearer {}", KEY))
        .header("X-Request-Id", "bad request id")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let generated = res.headers()["x-request-id"].to_str().unwrap();
    assert_ne!(generated, "bad request id");
    assert!(generated.bytes().all(|byte| byte.is_ascii_alphanumeric()));
}

#[tokio::test]
async fn api_keys_still_bypass_every_rule() {
    let (app, _dir) = test_app_with_identity().await;
    push_ruled_schema(
        &app,
        json!({"read": {"owner": "authorId"}, "write": {"owner": "authorId"}}),
    )
    .await;
    let base = format!("/v1/{}/posts", PROJECT);

    // Documents owned by nobody in particular, written with a project key.
    let (status, created) = send(&app, "POST", &base, Some(KEY), Some(owned_post("a", "u2"))).await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["_id"].as_str().unwrap().to_string();

    for (method, path, body) in [
        ("GET", format!("{}/{}", base, id), None),
        ("GET", base.clone(), None),
        (
            "PATCH",
            format!("{}/{}", base, id),
            Some(json!({"title": "edited"})),
        ),
    ] {
        let (status, _) = send(&app, method, &path, Some(KEY), body).await;
        assert_eq!(status, StatusCode::OK, "{} {}", method, path);
    }

    // Including documents an owner rule would have hidden.
    let (_, listed) = send(&app, "GET", &base, Some(KEY), None).await;
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn adversarial_tokens_are_refused_at_the_boundary() {
    let (app, _dir) = test_app_with_identity().await;
    push_ruled_schema(&app, json!({"read": "authenticated"})).await;
    let base = format!("/v1/{}/posts", PROJECT);

    let claims = json!({
        "iss": ISSUER, "sub": "u1",
        "exp": jsonwebtoken::get_current_timestamp() + 3600
    });

    // `alg: none`, hand-assembled since `encode` will not produce it.
    let unsigned = format!(
        "{}.{}.",
        base64_url(br#"{"alg":"none","typ":"JWT"}"#),
        base64_url(claims.to_string().as_bytes())
    );

    // HS256 signed with the public modulus as the HMAC secret: the classic
    // alg-confusion attack against a server that trusts the header.
    let mut hs = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
    hs.kid = Some(KID.to_string());
    let confused = jsonwebtoken::encode(
        &hs,
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(MODULUS.as_bytes()),
    )
    .unwrap();

    // Expired, and signed by an unknown key.
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(KID.to_string());
    let expired = jsonwebtoken::encode(
        &header,
        &json!({"iss": ISSUER, "sub": "u1", "exp": 1_000_000_000}),
        &jsonwebtoken::EncodingKey::from_rsa_pem(test_private_pem().as_bytes()).unwrap(),
    )
    .unwrap();

    let mut unknown_kid = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    unknown_kid.kid = Some("rotated-away".to_string());
    let wrong_key = jsonwebtoken::encode(
        &unknown_kid,
        &claims,
        &jsonwebtoken::EncodingKey::from_rsa_pem(test_private_pem().as_bytes()).unwrap(),
    )
    .unwrap();

    for (name, token) in [
        ("alg none", unsigned),
        ("alg confusion", confused),
        ("expired", expired),
        ("unknown kid", wrong_key),
        ("truncated", "a.b.c".to_string()),
    ] {
        let (status, _) = send(&app, "GET", &base, Some(&token), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{} was accepted", name);
    }
}

fn base64_url(bytes: &[u8]) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    URL_SAFE_NO_PAD.encode(bytes)
}

#[tokio::test]
async fn owner_field_type_confusion_denies() {
    let (app, _dir) = test_app_with_identity().await;
    // The owner field is deliberately *undeclared*, so the schema validator
    // lets any JSON type through and the rules layer is what must hold.
    let mut schema = ruled_schema(json!({"read": {"owner": "authorId"}}));
    schema["collections"]["posts"]["fields"]
        .as_object_mut()
        .unwrap()
        .remove("authorId");
    let (status, _) = send(
        &app,
        "PUT",
        &format!("/v1/{}/_schema", PROJECT),
        Some("admin-key"),
        Some(schema),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let base = format!("/v1/{}/posts", PROJECT);
    let u1 = token_for("u1");

    // Documents whose owner field is not a plain matching string. Written with
    // an API key, which bypasses rules, then read back as the end user.
    for owner in [json!(null), json!(42), json!(["u1"]), json!({"id": "u1"})] {
        let mut doc = searchable_post("x", "Post x", "Body about rust.");
        doc["authorId"] = owner.clone();
        let (status, created) = send(&app, "POST", &base, Some(KEY), Some(doc)).await;
        assert_eq!(status, StatusCode::CREATED, "owner: {}", owner);
        let id = created["_id"].as_str().unwrap().to_string();

        let (status, _) = send(&app, "GET", &format!("{}/{}", base, id), Some(&u1), None).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "owner {} was treated as a match",
            owner
        );
    }

    // A document with no owner field at all is likewise invisible.
    let (_, created) = send(
        &app,
        "POST",
        &base,
        Some(KEY),
        Some(searchable_post("none", "Post none", "Body about rust.")),
    )
    .await;
    let id = created["_id"].as_str().unwrap().to_string();
    let (status, _) = send(&app, "GET", &format!("{}/{}", base, id), Some(&u1), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // And none of them leak through the collection-wide reads.
    let (_, listed) = send(&app, "GET", &base, Some(&u1), None).await;
    assert!(listed["data"].as_array().unwrap().is_empty(), "{}", listed);
    let (_, found) = send(
        &app,
        "POST",
        &format!("{}/_search", base),
        Some(&u1),
        Some(json!({"query": "rust"})),
    )
    .await;
    assert!(found["data"].as_array().unwrap().is_empty(), "{}", found);
}

#[tokio::test]
async fn owner_filter_holds_across_query_and_deep_result_sets() {
    let (app, _dir) = test_app_with_identity().await;
    push_ruled_schema(&app, json!({"read": {"owner": "authorId"}})).await;
    let base = format!("/v1/{}/posts", PROJECT);

    // Many documents owned by someone else, ranking above the one owned page.
    for i in 0..25 {
        let mut doc = owned_post(&format!("theirs-{}", i), "u2");
        doc["title"] = json!("rust rust rust rust");
        send(&app, "POST", &base, Some(KEY), Some(doc)).await;
    }
    let mut mine = owned_post("mine", "u1");
    mine["title"] = json!("rust");
    send(&app, "POST", &base, Some(KEY), Some(mine)).await;

    let u1 = token_for("u1");

    // `_query` is post-filtered like `list` and `_search`.
    let (status, res) = send(
        &app,
        "POST",
        &format!("{}/_query", base),
        Some(&u1),
        Some(json!({"index": "by_status", "params": {"status": "published"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for doc in res["data"].as_array().unwrap() {
        assert_eq!(doc["authorId"], "u1", "query leaked: {}", doc);
    }

    // Search overfetches to refill an owner-filtered page, and never returns a
    // document the caller does not own even when 25 others outrank it.
    let (_, found) = send(
        &app,
        "POST",
        &format!("{}/_search", base),
        Some(&u1),
        Some(json!({"query": "rust", "limit": 5})),
    )
    .await;
    for doc in found["data"].as_array().unwrap() {
        assert_eq!(doc["authorId"], "u1", "search leaked: {}", doc);
    }

    // The same reads with an API key see everything, confirming the documents
    // are really there and the filter is what hid them.
    let (_, all) = send(&app, "GET", &base, Some(KEY), None).await;
    assert_eq!(all["data"].as_array().unwrap().len(), 26);
}

#[tokio::test]
async fn write_only_rules_do_not_grant_reads() {
    let (app, _dir) = test_app_with_identity().await;
    push_ruled_schema(&app, json!({"write": "authenticated"})).await;
    let base = format!("/v1/{}/posts", PROJECT);
    let u1 = token_for("u1");

    // Writing is allowed...
    let (status, created) = send(&app, "POST", &base, Some(&u1), Some(owned_post("a", "u1"))).await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["_id"].as_str().unwrap().to_string();

    // ...reading is not, for the same identity.
    for path in [base.clone(), format!("{}/{}", base, id)] {
        let (status, _) = send(&app, "GET", &path, Some(&u1), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "path: {}", path);
    }
    let (status, _) = send(
        &app,
        "POST",
        &format!("{}/_search", base),
        Some(&u1),
        Some(json!({"query": "rust"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn end_users_cannot_reach_collections_without_rules() {
    let (app, _dir) = test_app_with_identity().await;
    // `posts` gets rules; `authors` is declared but has none.
    let mut schema = ruled_schema(json!({"read": "public"}));
    schema["collections"]["authors"] = json!({
        "fields": {"name": {"type": "string"}}
    });
    let (status, _) = send(
        &app,
        "PUT",
        &format!("/v1/{}/_schema", PROJECT),
        Some("admin-key"),
        Some(schema),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let u1 = token_for("u1");
    let (status, _) = send(
        &app,
        "GET",
        &format!("/v1/{}/authors", PROJECT),
        Some(&u1),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(&app, "GET", &format!("/v1/{}/authors", PROJECT), None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A collection absent from the schema entirely is equally closed.
    let (status, _) = send(
        &app,
        "GET",
        &format!("/v1/{}/ghosts", PROJECT),
        Some(&u1),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn owner_check_uses_the_document_under_the_write_lock() {
    // The rule is evaluated inside the write lock, against the stored document,
    // so a decision cannot be raced by a concurrent transfer of ownership.
    // Here the transfer happens *before* the write rather than during it, which
    // exercises the same path deterministically: the pre-read the handler used
    // to rely on is gone, so the check sees the current owner.
    let (app, _dir) = test_app_with_identity().await;
    push_ruled_schema(
        &app,
        json!({"read": "public", "write": {"owner": "authorId"}}),
    )
    .await;
    let base = format!("/v1/{}/posts", PROJECT);
    let u1 = token_for("u1");

    let (_, created) = send(&app, "POST", &base, Some(KEY), Some(owned_post("a", "u1"))).await;
    let id = created["_id"].as_str().unwrap().to_string();

    // u1 owns it and may write.
    let (status, _) = send(
        &app,
        "PATCH",
        &format!("{}/{}", base, id),
        Some(&u1),
        Some(json!({"title": "mine"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // An API key transfers ownership away.
    let (status, _) = send(
        &app,
        "PATCH",
        &format!("{}/{}", base, id),
        Some(KEY),
        Some(json!({"authorId": "u2"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Every subsequent write by u1 is refused, on all three write paths.
    let (status, _) = send(
        &app,
        "PATCH",
        &format!("{}/{}", base, id),
        Some(&u1),
        Some(json!({"title": "not mine any more"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = send(
        &app,
        "PUT",
        &format!("{}/{}", base, id),
        Some(&u1),
        Some(owned_post("a", "u1")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = send(&app, "DELETE", &format!("{}/{}", base, id), Some(&u1), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---- the authorization decision is made under the write lock ----
//
// Both races below are decided by *when* the check reads state, so each test
// arranges for the pre-write state and the at-write state to differ and asserts
// the at-write state won. That is deterministic: no interleaving required.

#[tokio::test]
async fn stale_auth_payload_cannot_revert_an_admin_change() {
    // The gate compares the incoming `auth` against the schema being replaced.
    // A project-scoped key holding a payload that matched an *earlier* stored
    // schema must not be able to push it once an admin has moved auth on.
    let (app, _dir) = test_app_with_identity().await;
    let path = format!("/v1/{}/_schema", PROJECT);

    // Admin establishes auth = ISSUER.
    let original = ruled_schema(json!({"read": "public"}));
    let (status, _) = send(
        &app,
        "PUT",
        &path,
        Some("admin-key"),
        Some(original.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // A project key may re-push it unchanged: not a trust change.
    let (status, _) = send(&app, "PUT", &path, Some(KEY), Some(original.clone())).await;
    assert_eq!(status, StatusCode::OK);

    // Admin repoints auth at a second issuer.
    let mut moved = ruled_schema(json!({"read": "public"}));
    moved["auth"]["providers"][0]["issuer"] = json!("https://idp2.test");
    let (status, _) = send(&app, "PUT", &path, Some("admin-key"), Some(moved)).await;
    assert_eq!(status, StatusCode::OK);

    // The project key's once-valid payload now differs from what is stored, so
    // pushing it would revert the admin's change. Refused.
    let (status, _) = send(&app, "PUT", &path, Some(KEY), Some(original)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // And the admin's issuer is still the one in force.
    let (_, stored) = send(&app, "GET", &path, Some("admin-key"), None).await;
    assert_eq!(
        stored["auth"]["providers"][0]["issuer"],
        "https://idp2.test"
    );
}

#[tokio::test]
async fn writes_are_judged_by_the_rules_in_force_at_write_time() {
    let (app, _dir) = test_app_with_identity().await;
    let base = format!("/v1/{}/posts", PROJECT);
    let u1 = token_for("u1");

    // Start permissive: any authenticated user may write anything.
    push_ruled_schema(&app, json!({"read": "public", "write": "authenticated"})).await;
    let (status, created) = send(&app, "POST", &base, Some(&u1), Some(owned_post("a", "u2"))).await;
    assert_eq!(status, StatusCode::CREATED, "setup write should be allowed");
    let id = created["_id"].as_str().unwrap().to_string();

    // Tighten to an owner rule. The document is owned by u2, not u1.
    push_ruled_schema(
        &app,
        json!({"read": "public", "write": {"owner": "authorId"}}),
    )
    .await;

    // Every write path must now consult the tightened rule.
    let (status, _) = send(&app, "POST", &base, Some(&u1), Some(owned_post("b", "u2"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "create used a stale rule");

    let (status, _) = send(
        &app,
        "PATCH",
        &format!("{}/{}", base, id),
        Some(&u1),
        Some(json!({"title": "edited"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "patch used a stale rule");

    let (status, _) = send(
        &app,
        "PUT",
        &format!("{}/{}", base, id),
        Some(&u1),
        Some(owned_post("a", "u1")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "replace used a stale rule");

    let (status, _) = send(&app, "DELETE", &format!("{}/{}", base, id), Some(&u1), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "delete used a stale rule");

    // Loosening again re-permits, proving the check tracks the schema rather
    // than having latched.
    push_ruled_schema(&app, json!({"read": "public", "write": "authenticated"})).await;
    let (status, _) = send(
        &app,
        "PATCH",
        &format!("{}/{}", base, id),
        Some(&u1),
        Some(json!({"title": "edited"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// A bare `Db`, for exercising the write-lock preconditions directly. Going
/// through HTTP cannot distinguish "checked before the write" from "checked
/// during it" without real interleaving; calling the checked writes directly
/// can, because the predicate reports what it was handed.
async fn test_db() -> (Db, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        host: "127.0.0.1".to_string(),
        port: 0,
        storage: StorageBackend::Local {
            path: dir.path().to_str().unwrap().to_string(),
        },
        db_path: "test".to_string(),
        api_keys: HashMap::new(),
        keybroker: None,
        embeddings: None,
    };
    let db = Db::open(&config).await.unwrap();
    (db, dir)
}

fn schema_with_rules(rules: Value) -> brickbed_server::schema::ProjectSchema {
    let mut schema = search_schema_body();
    schema["collections"]["posts"]["rules"] = rules;
    serde_json::from_value(schema).unwrap()
}

#[tokio::test]
async fn document_precondition_receives_the_rules_stored_at_write_time() {
    let (db, _dir) = test_db().await;
    db.put_schema(PROJECT, &schema_with_rules(json!({"read": "public"})))
        .await
        .unwrap();

    let data = searchable_post("a", "Post a", "Body about rust");
    let doc = db
        .insert(PROJECT, "posts", data.as_object().unwrap().clone())
        .await
        .unwrap();

    // The rule changes after the document exists. A decision captured before
    // the write would still be the old one; the predicate must see the new.
    //
    // LIMITATION (accepted at review): this sequences the schema change before
    // the write rather than interleaving inside it, so an implementation that
    // read the schema immediately BEFORE taking the lock would also pass.
    // True pinning needs a fault-injection hook in the lock path; the
    // under-lock property itself was verified by review of db.rs.
    db.put_schema(
        PROJECT,
        &schema_with_rules(json!({"write": {"owner": "authorId"}})),
    )
    .await
    .unwrap();

    let seen = std::sync::Mutex::new(None);
    let observe = |ctx: brickbed_server::db::PreconditionCtx<'_>| {
        *seen.lock().unwrap() = Some((
            ctx.collection.and_then(|c| c.rules.clone()),
            ctx.existing.is_some(),
            ctx.next.is_some(),
        ));
        true
    };

    db.replace_checked(
        PROJECT,
        "posts",
        &doc.id,
        data.as_object().unwrap().clone(),
        Some(&observe),
    )
    .await
    .unwrap();

    let (rules, had_existing, had_next) = seen.lock().unwrap().clone().expect("never called");
    let rules = rules.expect("collection rules missing");
    assert!(
        rules.write.is_some() && rules.read.is_none(),
        "predicate saw the pre-change rules: {:?}",
        rules
    );
    assert!(had_existing && had_next, "update needs both sides");

    // Returning false aborts the write.
    let refuse = |_: brickbed_server::db::PreconditionCtx<'_>| false;
    let err = db
        .replace_checked(
            PROJECT,
            "posts",
            &doc.id,
            json!({"title": "clobbered"}).as_object().unwrap().clone(),
            Some(&refuse),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, brickbed_server::error::AppError::Forbidden));
    let after = db.get(PROJECT, "posts", &doc.id).await.unwrap();
    assert_eq!(after.data["title"], "Post a", "refused write still landed");
}

#[tokio::test]
async fn schema_precondition_receives_the_schema_being_replaced() {
    let (db, _dir) = test_db().await;
    let first = schema_with_rules(json!({"read": "public"}));
    db.put_schema(PROJECT, &first).await.unwrap();

    let seen = std::sync::Mutex::new(None);
    let observe = |stored: Option<&brickbed_server::schema::ProjectSchema>| {
        *seen.lock().unwrap() = Some(stored.and_then(|s| s.rules("posts").cloned()));
        true
    };
    let second = schema_with_rules(json!({"read": "authenticated"}));
    db.put_schema_checked(PROJECT, &second, Some(&observe))
        .await
        .unwrap();

    let stored = seen.lock().unwrap().clone().expect("never called");
    let stored = stored.expect("stored rules missing");
    assert_eq!(
        stored.read,
        Some(brickbed_server::rules::Rule::Public),
        "predicate saw something other than the schema it replaced"
    );

    // Returning false aborts the push, leaving the stored schema alone.
    let refuse = |_: Option<&brickbed_server::schema::ProjectSchema>| false;
    let third = schema_with_rules(json!({"read": {"owner": "authorId"}}));
    let err = db
        .put_schema_checked(PROJECT, &third, Some(&refuse))
        .await
        .unwrap_err();
    assert!(matches!(err, brickbed_server::error::AppError::Forbidden));

    let after = db.get_schema(PROJECT).await.unwrap().unwrap();
    assert_eq!(
        after.rules("posts").unwrap().read,
        Some(brickbed_server::rules::Rule::Authenticated),
        "refused push still landed"
    );
}

#[tokio::test]
async fn owner_rules_cannot_match_on_a_server_filled_field() {
    // Embeddings are written by the server after the rule is evaluated, so an
    // owner rule pointing at a vector field would judge a document that is not
    // the one stored. Rejected at push, where it is fixable.
    let (app, _dir) = test_app_with_identity().await;
    let path = format!("/v1/{}/_schema", PROJECT);

    let mut schema = ruled_schema(json!({"read": {"owner": "embedding"}}));
    schema["collections"]["posts"]["fields"]["embedding"] =
        json!({"type": "vector", "dims": 3, "from": ["title"], "model": "m"});
    let (status, err) = send(&app, "PUT", &path, Some("admin-key"), Some(schema)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        error_message(&err).contains("server-filled"),
        "error: {}",
        err["error"]
    );

    // The same rule on an ordinary field is fine.
    let mut schema = ruled_schema(json!({"read": {"owner": "authorId"}}));
    schema["collections"]["posts"]["fields"]["embedding"] =
        json!({"type": "vector", "dims": 3, "from": ["title"], "model": "m"});
    let (status, _) = send(&app, "PUT", &path, Some("admin-key"), Some(schema)).await;
    assert_eq!(status, StatusCode::OK);
}

/// Counts provider calls so ordering bugs (embedding before validation) show
/// up as a nonzero count rather than only as latency and spend.
struct CountingProvider {
    inner: brickbed_server::embed::MockProvider,
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl EmbeddingProvider for CountingProvider {
    fn name(&self) -> &'static str {
        "counting-mock"
    }
    async fn embed(
        &self,
        texts: &[String],
        model: &str,
    ) -> Result<Vec<Vec<f32>>, brickbed_server::error::AppError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.embed(texts, model).await
    }
}

#[tokio::test]
async fn schema_invalid_writes_never_call_the_embedding_provider() {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider = CountingProvider {
        inner: brickbed_server::embed::MockProvider::new(3),
        calls: calls.clone(),
    };
    let (app, _dir) = test_app_with_embedder(Some(Arc::new(provider))).await;

    let mut schema = vector_schema_body();
    schema["collections"]["posts"]["fields"]["embedding"] = json!({
        "type": "optional",
        "inner": {"type": "vector", "dims": 3, "from": ["title", "body"], "model": "m"}
    });
    push_schema(&app, schema).await;

    // Invalid document (title must be a string): rejected before any provider call.
    let mut bad = searchable_post("bad", "Valid title", "Body.");
    bad["title"] = json!(123);
    let (status, _) = send(
        &app,
        "POST",
        &format!("/v1/{}/posts", PROJECT),
        Some(KEY),
        Some(bad),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);

    // A valid document embeds exactly once.
    create_post(&app, searchable_post("good", "Valid title", "Body.")).await;
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unknown_keys_in_rules_and_auth_are_push_errors() {
    let (app, _dir) = test_app().await;

    // Typo'd rules key: must be a 4xx, never a silently different policy.
    let mut schema = schema_body();
    schema["collections"]["posts"]["rules"] = json!({"reaad": "public"});
    let (status, err) = send(
        &app,
        "PUT",
        &format!("/v1/{}/_schema", PROJECT),
        Some(KEY),
        Some(schema),
    )
    .await;
    assert!(
        status.is_client_error(),
        "typo'd rules key accepted: {} {:?}",
        status,
        err
    );

    // Typo'd provider key ("audiance") must not silently skip the aud check.
    let mut schema = schema_body();
    schema["auth"] = json!({"providers": [{
        "issuer": "https://issuer.example.com",
        "audiance": "my-app"
    }]});
    let (status, err) = send(
        &app,
        "PUT",
        &format!("/v1/{}/_schema", PROJECT),
        Some("admin-key"),
        Some(schema),
    )
    .await;
    assert!(
        status.is_client_error(),
        "typo'd provider key accepted: {} {:?}",
        status,
        err
    );
}

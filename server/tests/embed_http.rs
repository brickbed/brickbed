//! Embed-on-write over real HTTP, against a mock provider on loopback.
//!
//! The unit tests parse captured provider responses, and the live smoke tests
//! need credentials and egress. Neither exercises the request the client
//! *builds*. These do: a real reqwest call leaves the process, and the server
//! on the other end asserts the URL, the auth header and the body.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use tower::ServiceExt;

use brickbed_server::build_router;
use brickbed_server::config::{Config, StorageBackend};
use brickbed_server::db::Db;
use brickbed_server::embed::{CohereProvider, EmbeddingProvider, OpenAiProvider};
use brickbed_server::handlers::AppState;
use brickbed_server::jwt::{HttpJwksFetcher, JwksCache};

const KEY: &str = "test-key";
const PROJECT: &str = "testproj";
const PROVIDER_KEY: &str = "provider-secret-123";

/// What the mock provider saw.
#[derive(Default)]
struct Seen {
    requests: usize,
    authorization: Option<String>,
    body: Option<Value>,
}

type Shared = Arc<Mutex<Seen>>;

fn record(state: &Shared, headers: &HeaderMap, body: &Value) {
    let mut seen = state.lock().unwrap();
    seen.requests += 1;
    seen.authorization = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    seen.body = Some(body.clone());
}

async fn openai_route(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    record(&state, &headers, &body);
    let n = body["input"].as_array().map_or(0, Vec::len);
    let data: Vec<Value> = (0..n)
        .map(|i| json!({"object": "embedding", "index": i, "embedding": [0.1, 0.2, 0.3, 0.4]}))
        .collect();
    Json(json!({"object": "list", "data": data, "model": "mock", "usage": {}}))
}

async fn cohere_route(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    record(&state, &headers, &body);
    let n = body["texts"].as_array().map_or(0, Vec::len);
    let vectors: Vec<Value> = (0..n).map(|_| json!([0.5, 0.6, 0.7, 0.8])).collect();
    Json(
        json!({"id": "mock", "embeddings": {"float": vectors}, "response_type": "embeddings_by_type"}),
    )
}

/// Serve one provider route on loopback and return its base URL.
async fn serve(path: &str, handler: axum::routing::MethodRouter<Shared>) -> (String, Shared) {
    let state: Shared = Arc::new(Mutex::new(Seen::default()));
    let app = Router::new().route(path, handler).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{}", addr), state)
}

async fn app_with(embedder: Arc<dyn EmbeddingProvider>) -> (Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        host: "127.0.0.1".to_string(),
        port: 0,
        storage: StorageBackend::Local {
            path: dir.path().to_str().unwrap().to_string(),
        },
        db_path: "test".to_string(),
        api_keys: HashMap::from([(KEY.to_string(), PROJECT.to_string())]),
        keybroker: None,
        embeddings: None,
    };
    let db = Db::open_with_embedder(&config, Some(embedder))
        .await
        .unwrap();
    let state = Arc::new(AppState {
        db,
        api_keys: config.api_keys.clone(),
        keybroker: None,
        jwks: JwksCache::new(HttpJwksFetcher::new()),
    });
    (build_router(state), dir)
}

async fn send(app: &Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("Authorization", format!("Bearer {}", KEY));
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
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

fn embed_schema() -> Value {
    json!({
        "collections": {
            "notes": {
                "fields": {
                    "title": {"type": "string"},
                    "body": {"type": "string"},
                    "embedding": {
                        "type": "vector",
                        "dims": 4,
                        "from": ["title", "body"],
                        "model": "test-model"
                    }
                }
            }
        }
    })
}

/// Vectors are stored as f32, so a provider's `0.1` comes back as
/// 0.10000000149…; compare on value, not on representation.
fn assert_vector(actual: &Value, expected: &[f64]) {
    let actual: Vec<f64> = actual
        .as_array()
        .unwrap_or_else(|| panic!("expected a stored vector, got {}", actual))
        .iter()
        .map(|c| c.as_f64().expect("numeric component"))
        .collect();
    assert_eq!(actual.len(), expected.len(), "width: {:?}", actual);
    for (got, want) in actual.iter().zip(expected) {
        assert!(
            (got - want).abs() < 1e-6,
            "component {} != {} in {:?}",
            got,
            want,
            actual
        );
    }
}

async fn insert_note(app: &Router) -> Value {
    let (status, created) = send(
        app,
        "POST",
        &format!("/v1/{}/notes", PROJECT),
        Some(json!({"title": "Rust storage", "body": "A fast engine."})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "insert failed: {}", created);
    created
}

#[tokio::test]
async fn openai_request_is_built_correctly() {
    let (base, seen) = serve("/v1/embeddings", post(openai_route)).await;
    let provider = OpenAiProvider::new(PROVIDER_KEY.to_string(), Some(base)).unwrap();
    let (app, _dir) = app_with(Arc::new(provider)).await;

    let (status, _) = send(
        &app,
        "PUT",
        &format!("/v1/{}/_schema", PROJECT),
        Some(embed_schema()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let created = insert_note(&app).await;
    assert_vector(&created["embedding"], &[0.1, 0.2, 0.3, 0.4]);

    let seen = seen.lock().unwrap();
    assert_eq!(seen.requests, 1, "one document, one provider call");
    assert_eq!(
        seen.authorization.as_deref(),
        Some(&format!("Bearer {}", PROVIDER_KEY)[..]),
        "bearer auth must reach the provider"
    );
    let body = seen.body.as_ref().unwrap();
    assert_eq!(body["model"], "test-model", "model comes from the schema");
    // Source fields are concatenated in declaration order into one input.
    assert_eq!(body["input"], json!(["Rust storage A fast engine."]));
}

#[tokio::test]
async fn cohere_request_is_built_correctly() {
    let (base, seen) = serve("/v2/embed", post(cohere_route)).await;
    let provider = CohereProvider::new(PROVIDER_KEY.to_string(), Some(base)).unwrap();
    let (app, _dir) = app_with(Arc::new(provider)).await;

    let (status, _) = send(
        &app,
        "PUT",
        &format!("/v1/{}/_schema", PROJECT),
        Some(embed_schema()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let created = insert_note(&app).await;
    assert_vector(&created["embedding"], &[0.5, 0.6, 0.7, 0.8]);

    let seen = seen.lock().unwrap();
    assert_eq!(seen.requests, 1);
    assert_eq!(
        seen.authorization.as_deref(),
        Some(&format!("Bearer {}", PROVIDER_KEY)[..])
    );
    let body = seen.body.as_ref().unwrap();
    assert_eq!(body["model"], "test-model");
    assert_eq!(body["texts"], json!(["Rust storage A fast engine."]));
    // v2 needs these two or it answers with a different shape.
    assert_eq!(body["input_type"], "search_document");
    assert_eq!(body["embedding_types"], json!(["float"]));
}

#[tokio::test]
async fn a_provider_error_is_surfaced_as_502_without_the_key() {
    // A provider that echoes the request back in its error body, which is how
    // a misconfigured proxy could leak the credential it was sent.
    async fn echoing_error(headers: HeaderMap, Json(_): Json<Value>) -> (StatusCode, String) {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        (
            StatusCode::UNAUTHORIZED,
            format!("{{\"error\":\"bad token\",\"sent\":\"{}\"}}", auth),
        )
    }

    let state: Shared = Arc::new(Mutex::new(Seen::default()));
    let app = Router::new()
        .route("/v1/embeddings", post(echoing_error))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let provider =
        OpenAiProvider::new(PROVIDER_KEY.to_string(), Some(format!("http://{}", addr))).unwrap();
    let (app, _dir) = app_with(Arc::new(provider)).await;

    let (status, _) = send(
        &app,
        "PUT",
        &format!("/v1/{}/_schema", PROJECT),
        Some(embed_schema()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, err) = send(
        &app,
        "POST",
        &format!("/v1/{}/notes", PROJECT),
        Some(json!({"title": "Rust storage", "body": "A fast engine."})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);

    let message = err["error"].as_str().unwrap();
    assert!(message.contains("401"), "message: {}", message);
    assert!(
        !message.contains(PROVIDER_KEY),
        "the key came back in the provider's body and must be scrubbed: {}",
        message
    );
    assert!(message.contains("[redacted]"), "message: {}", message);
}

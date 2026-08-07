//! Live embedding-provider smoke tests.
//!
//! Ignored by default: these call a real API and need a key in the
//! environment. They exist because the mock cannot catch a wrong field name
//! or response envelope — only a real round trip can.
//!
//! ```text
//! cargo test --test live_smoke openai -- --ignored --nocapture
//! cargo test --test live_smoke cohere -- --ignored --nocapture
//! ```
//!
//! Each test embeds three short documents through the ordinary write path.

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
use brickbed_server::embed::{CohereProvider, EmbeddingProvider, OpenAiProvider};
use brickbed_server::handlers::AppState;
use brickbed_server::jwt::{HttpJwksFetcher, JwksCache};

const KEY: &str = "test-key";
const PROJECT: &str = "testproj";

async fn app_with(embedder: Arc<dyn EmbeddingProvider>) -> (Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        host: "127.0.0.1".to_string(),
        port: 0,
        storage: StorageBackend::Local {
            path: dir.path().to_str().unwrap().to_string(),
        },
        db_path: "live".to_string(),
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

fn schema(dims: usize, model: &str) -> Value {
    json!({
        "collections": {
            "notes": {
                "fields": {
                    "title": {"type": "string"},
                    "body": {"type": "string"},
                    "slug": {"type": "string"},
                    "embedding": {
                        "type": "vector",
                        "dims": dims,
                        "from": ["title", "body"],
                        "model": model
                    }
                },
                "vectorIndexes": [
                    {"name": "by_embedding", "field": "embedding", "metric": "cosine", "dims": dims}
                ]
            }
        }
    })
}

/// Three short documents: two about the same subject, one unrelated.
fn documents() -> Vec<Value> {
    vec![
        json!({
            "slug": "postgres",
            "title": "Tuning Postgres queries",
            "body": "Index selection and reading query plans in a relational database."
        }),
        json!({
            "slug": "mysql",
            "title": "Optimising MySQL queries",
            "body": "Choosing indexes and reading query plans in a relational database."
        }),
        json!({
            "slug": "bread",
            "title": "Baking sourdough bread",
            "body": "Starter hydration and oven temperature for a crusty loaf."
        }),
    ]
}

fn cosine(a: &[f64], b: &[f64]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f64 = a.iter().map(|x| x * x).sum::<f64>().sqrt();
    let nb: f64 = b.iter().map(|x| x * x).sum::<f64>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na * nb)
}

/// Insert the documents through the real write path and check that the stored
/// vectors have the declared width and that similarity tracks meaning.
async fn smoke(provider: Arc<dyn EmbeddingProvider>, dims: usize, model: &str) {
    let name = provider.name();
    let (app, _dir) = app_with(provider).await;

    let (status, err) = send(
        &app,
        "PUT",
        &format!("/v1/{}/_schema", PROJECT),
        Some(schema(dims, model)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "schema push failed: {}", err);

    let mut vectors: HashMap<String, Vec<f64>> = HashMap::new();
    for doc in documents() {
        let slug = doc["slug"].as_str().unwrap().to_string();
        let (status, created) =
            send(&app, "POST", &format!("/v1/{}/notes", PROJECT), Some(doc)).await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "{} insert failed for {:?}: {}",
            name,
            slug,
            created
        );

        let embedding: Vec<f64> = created["embedding"]
            .as_array()
            .unwrap_or_else(|| panic!("{}: no server-filled embedding on {:?}", name, slug))
            .iter()
            .map(|c| c.as_f64().expect("numeric component"))
            .collect();

        assert_eq!(
            embedding.len(),
            dims,
            "{}: model {:?} returned {} dimensions",
            name,
            model,
            embedding.len()
        );
        assert!(
            embedding.iter().all(|c| c.is_finite()),
            "{}: non-finite component",
            name
        );
        assert!(
            embedding.iter().any(|c| *c != 0.0),
            "{}: all-zero vector",
            name
        );
        vectors.insert(slug, embedding);
    }

    // Two documents about databases must sit closer together than either does
    // to one about bread. This is what proves the response was parsed into the
    // right order and shape rather than merely being the right length.
    let related = cosine(&vectors["postgres"], &vectors["mysql"]);
    let unrelated_a = cosine(&vectors["postgres"], &vectors["bread"]);
    let unrelated_b = cosine(&vectors["mysql"], &vectors["bread"]);
    println!(
        "{} / {}: related={:.4} unrelated={:.4},{:.4}",
        name, model, related, unrelated_a, unrelated_b
    );
    assert!(
        related > unrelated_a && related > unrelated_b,
        "{}: similarity does not track meaning (related {:.4}, unrelated {:.4}/{:.4})",
        name,
        related,
        unrelated_a,
        unrelated_b
    );

    // The vectors reached the index: searching by one document's own vector
    // returns it first.
    let (status, res) = send(
        &app,
        "POST",
        &format!("/v1/{}/notes/_search", PROJECT),
        Some(json!({"vector": vectors["postgres"], "limit": 2})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "search failed: {}", res);
    assert_eq!(res["data"][0]["slug"], "postgres");
}

#[tokio::test]
#[ignore = "calls the live OpenAI API; needs OPENAI_API_KEY"]
async fn openai_embeddings_round_trip() {
    let key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY required");
    let provider = OpenAiProvider::new(key, None).unwrap();
    smoke(Arc::new(provider), 1536, "text-embedding-3-small").await;
}

#[tokio::test]
#[ignore = "calls the live Cohere API; needs COHERE_API_KEY"]
async fn cohere_embeddings_round_trip() {
    let key = std::env::var("COHERE_API_KEY").expect("COHERE_API_KEY required");
    let provider = CohereProvider::new(key, None).unwrap();
    smoke(Arc::new(provider), 1536, "embed-v4.0").await;
}

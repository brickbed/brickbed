//! Concurrency properties of the write path.
//!
//! The BM25 corpus stats entry is a read-modify-write, so concurrent writers
//! must be serialised across it or counts are lost. The lock that guarantees
//! that used to be held across the durable flush too, which serialised every
//! write in the server; these tests pin the property the lock exists for, so
//! the durability half can stay outside it.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use brickbed_server::build_router;
use brickbed_server::config::{Config, StorageBackend};
use brickbed_server::db::Db;
use brickbed_server::handlers::AppState;
use brickbed_server::jwt::{HttpJwksFetcher, JwksCache};

const KEY: &str = "test-key";
const PROJECT: &str = "testproj";
/// Every document below carries this many indexable tokens.
const TOKENS_PER_DOC: u64 = 4;

async fn test_app() -> (Router, Arc<AppState>, tempfile::TempDir) {
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
    let db = Db::open(&config).await.unwrap();
    let state = Arc::new(AppState {
        db,
        api_keys: config.api_keys.clone(),
        keybroker: None,
        jwks: JwksCache::new(HttpJwksFetcher::new()),
    });
    (build_router(state.clone()), state, dir)
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

fn schema(with_search: bool) -> Value {
    let mut collection = json!({
        "fields": {
            "title": {"type": "string"},
            "slug": {"type": "string"}
        },
        "indexes": [{"name": "by_slug", "fields": ["slug"]}]
    });
    if with_search {
        collection["searchIndexes"] = json!([{"name": "search", "fields": ["title"]}]);
    }
    json!({"collections": {"posts": collection}})
}

/// Four tokens after stopword removal, so corpus length is predictable.
fn doc(n: usize) -> Value {
    json!({"title": "rust storage engine benchmark", "slug": format!("doc-{}", n)})
}

/// Fire `n` writes at once and return every response status.
async fn concurrently<F, Fut>(n: usize, make: F) -> Vec<StatusCode>
where
    F: Fn(usize) -> Fut,
    Fut: std::future::Future<Output = StatusCode> + Send + 'static,
{
    let mut set = tokio::task::JoinSet::new();
    for i in 0..n {
        set.spawn(make(i));
    }
    let mut out = Vec::with_capacity(n);
    while let Some(joined) = set.join_next().await {
        out.push(joined.unwrap());
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_inserts_keep_corpus_stats_exact() {
    let (app, state, _dir) = test_app().await;
    let (status, _) = send(
        &app,
        "PUT",
        &format!("/v1/{}/_schema", PROJECT),
        Some(schema(true)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    const WRITERS: usize = 32;
    let statuses = concurrently(WRITERS, |i| {
        let app = app.clone();
        async move {
            send(
                &app,
                "POST",
                &format!("/v1/{}/posts", PROJECT),
                Some(doc(i)),
            )
            .await
            .0
        }
    })
    .await;
    assert!(
        statuses.iter().all(|s| *s == StatusCode::CREATED),
        "statuses: {:?}",
        statuses
    );

    // A lost update in the read-modify-write shows up here as a short count.
    let (docs, tokens) = state
        .db
        .search_corpus_stats(PROJECT, "posts", "search")
        .await
        .unwrap();
    assert_eq!(docs, WRITERS as u64, "corpus lost documents");
    assert_eq!(
        tokens,
        WRITERS as u64 * TOKENS_PER_DOC,
        "corpus lost tokens"
    );

    // Every document is durably readable, not merely acknowledged.
    let (status, listed) = send(
        &app,
        "GET",
        &format!("/v1/{}/posts?limit=1000", PROJECT),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["data"].as_array().unwrap().len(), WRITERS);

    // And searchable: all of them match the shared term.
    let (status, found) = send(
        &app,
        "POST",
        &format!("/v1/{}/posts/_search", PROJECT),
        Some(json!({"query": "rust", "limit": 100})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(found["data"].as_array().unwrap().len(), WRITERS);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_deletes_and_patches_keep_stats_exact() {
    let (app, state, _dir) = test_app().await;
    let (status, _) = send(
        &app,
        "PUT",
        &format!("/v1/{}/_schema", PROJECT),
        Some(schema(true)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    const TOTAL: usize = 16;
    let mut ids = Vec::new();
    for i in 0..TOTAL {
        let (status, created) = send(
            &app,
            "POST",
            &format!("/v1/{}/posts", PROJECT),
            Some(doc(i)),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        ids.push(created["_id"].as_str().unwrap().to_string());
    }

    // Half deleted, half patched, all at once: deletes subtract from the
    // corpus while patches replace one document's contribution.
    let ids = Arc::new(ids);
    let statuses = concurrently(TOTAL, |i| {
        let app = app.clone();
        let ids = ids.clone();
        async move {
            let path = format!("/v1/{}/posts/{}", PROJECT, ids[i]);
            if i % 2 == 0 {
                send(&app, "DELETE", &path, None).await.0
            } else {
                send(
                    &app,
                    "PATCH",
                    &path,
                    Some(json!({"title": "rust storage engine rewritten"})),
                )
                .await
                .0
            }
        }
    })
    .await;
    assert!(
        statuses
            .iter()
            .all(|s| *s == StatusCode::NO_CONTENT || *s == StatusCode::OK),
        "statuses: {:?}",
        statuses
    );

    let remaining = (TOTAL / 2) as u64;
    let (docs, tokens) = state
        .db
        .search_corpus_stats(PROJECT, "posts", "search")
        .await
        .unwrap();
    assert_eq!(docs, remaining, "deletes did not subtract exactly once");
    assert_eq!(tokens, remaining * TOKENS_PER_DOC, "token count drifted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writes_without_a_search_index_are_durable() {
    // This collection keeps no corpus stats, so the write path takes no lock
    // at all. The writes must still all land and be readable afterwards.
    let (app, _state, _dir) = test_app().await;
    let (status, _) = send(
        &app,
        "PUT",
        &format!("/v1/{}/_schema", PROJECT),
        Some(schema(false)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    const WRITERS: usize = 32;
    let statuses = concurrently(WRITERS, |i| {
        let app = app.clone();
        async move {
            send(
                &app,
                "POST",
                &format!("/v1/{}/posts", PROJECT),
                Some(doc(i)),
            )
            .await
            .0
        }
    })
    .await;
    assert!(
        statuses.iter().all(|s| *s == StatusCode::CREATED),
        "statuses: {:?}",
        statuses
    );

    let (status, listed) = send(
        &app,
        "GET",
        &format!("/v1/{}/posts?limit=1000", PROJECT),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["data"].as_array().unwrap().len(), WRITERS);

    // Each document's equality index entry is present too.
    for i in 0..WRITERS {
        let (status, found) = send(
            &app,
            "POST",
            &format!("/v1/{}/posts/_query", PROJECT),
            Some(json!({"index": "by_slug", "params": {"slug": format!("doc-{}", i)}})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(found["data"].as_array().unwrap().len(), 1, "doc-{}", i);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writes_to_one_document_leave_no_orphan_entries() {
    let (app, _state, _dir) = test_app().await;
    let (status, _) = send(
        &app,
        "PUT",
        &format!("/v1/{}/_schema", PROJECT),
        Some(schema(true)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, created) = send(
        &app,
        "POST",
        &format!("/v1/{}/posts", PROJECT),
        Some(json!({"title": "token00 storage engine benchmark", "slug": "original"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["_id"].as_str().unwrap().to_string();

    // Every writer retires the entries of the document it read and writes its
    // own. Running from the same snapshot, each would retire only the
    // original's, leaving every loser's postings and index entries behind.
    const WRITERS: usize = 12;
    let id = Arc::new(id);
    let statuses = concurrently(WRITERS, |i| {
        let app = app.clone();
        let id = id.clone();
        async move {
            send(
                &app,
                "PUT",
                &format!("/v1/{}/posts/{}", PROJECT, id),
                Some(json!({
                    // Zero-padded so no writer's term is a prefix of another's:
                    // "token1" would otherwise look like a match for "token12".
                    "title": format!("token{:02} storage engine benchmark", i + 1),
                    "slug": format!("slug-{}", i + 1)
                })),
            )
            .await
            .0
        }
    })
    .await;
    assert!(
        statuses.iter().all(|s| *s == StatusCode::OK),
        "statuses: {:?}",
        statuses
    );

    // Whichever writer landed last, exactly its term may remain indexed. Which
    // one wins depends on scheduling, so compare against the surviving
    // document rather than assuming an outcome.
    let (_, doc) = send(&app, "GET", &format!("/v1/{}/posts/{}", PROJECT, id), None).await;
    let winner = doc["title"].as_str().unwrap().to_string();
    let winning_slug = doc["slug"].as_str().unwrap().to_string();
    let winning_term = winner
        .split_whitespace()
        .next()
        .expect("title carries a term")
        .to_string();

    for i in 0..=WRITERS {
        let term = format!("token{:02}", i);
        // Exact token equality: the document holds this term or it does not.
        let expected = usize::from(winning_term == term);
        let (status, found) = send(
            &app,
            "POST",
            &format!("/v1/{}/posts/_search", PROJECT),
            Some(json!({"query": term, "limit": 100})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            found["data"].as_array().unwrap().len(),
            expected,
            "orphaned postings for {:?} (winner {:?})",
            term,
            winner
        );
    }

    // Same for the equality index: one entry, pointing at the surviving slug.
    for i in 0..=WRITERS {
        let slug = if i == 0 {
            "original".to_string()
        } else {
            format!("slug-{}", i)
        };
        let expected = usize::from(slug == winning_slug);
        let (status, found) = send(
            &app,
            "POST",
            &format!("/v1/{}/posts/_query", PROJECT),
            Some(json!({"index": "by_slug", "params": {"slug": slug}})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            found["data"].as_array().unwrap().len(),
            expected,
            "orphaned index entry for slug {:?}",
            slug
        );
    }

    // And the corpus still counts exactly one document.
    let (docs, tokens) = _state
        .db
        .search_corpus_stats(PROJECT, "posts", "search")
        .await
        .unwrap();
    assert_eq!(docs, 1, "corpus counted the update as extra documents");
    assert_eq!(tokens, TOKENS_PER_DOC);
}

/// Two writers race to claim the same document, each allowed only if the
/// document still belongs to the previous owner. Exactly one may win: the
/// loser's predicate has to see the winner's write, not the snapshot it
/// started from. This is the property that makes the checked writes usable
/// for ownership transfer at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_owner_transfers_serialise() {
    use brickbed_server::db::PreconditionCtx;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let (app, state, _dir) = test_app().await;
    let (status, _) = send(
        &app,
        "PUT",
        &format!("/v1/{}/_schema", PROJECT),
        Some(json!({"collections": {"posts": {
            "fields": {"title": {"type": "string"}, "slug": {"type": "string"}, "owner": {"type": "string"}},
            "indexes": [{"name": "by_slug", "fields": ["slug"]}],
            "searchIndexes": [{"name": "search", "fields": ["title"]}]
        }}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let mut doc = doc(0);
    doc["owner"] = json!("alice");
    let created = state
        .db
        .insert(PROJECT, "posts", doc.as_object().unwrap().clone())
        .await
        .unwrap();
    let id = Arc::new(created.id);

    // Both claimants demand that alice still owns the document.
    const CLAIMANTS: usize = 8;
    let wins = Arc::new(AtomicUsize::new(0));
    let forbidden = Arc::new(AtomicUsize::new(0));

    let mut set = tokio::task::JoinSet::new();
    for i in 0..CLAIMANTS {
        let state = state.clone();
        let id = id.clone();
        let wins = wins.clone();
        let forbidden = forbidden.clone();
        set.spawn(async move {
            let owned_by_alice = |ctx: PreconditionCtx<'_>| {
                ctx.existing
                    .and_then(|d| d.get("owner"))
                    .and_then(Value::as_str)
                    == Some("alice")
            };
            let patch = json!({"owner": format!("claimant-{}", i)});
            let result = state
                .db
                .patch_checked(
                    PROJECT,
                    "posts",
                    &id,
                    patch.as_object().unwrap().clone(),
                    Some(&owned_by_alice),
                )
                .await;
            match result {
                Ok(_) => wins.fetch_add(1, Ordering::SeqCst),
                Err(_) => forbidden.fetch_add(1, Ordering::SeqCst),
            };
        });
    }
    while let Some(joined) = set.join_next().await {
        joined.unwrap();
    }

    assert_eq!(
        wins.load(Ordering::SeqCst),
        1,
        "exactly one claimant may take a document owned by alice"
    );
    assert_eq!(forbidden.load(Ordering::SeqCst), CLAIMANTS - 1);

    // The surviving owner is one of the claimants, never alice.
    let stored = state.db.get(PROJECT, "posts", &id).await.unwrap();
    let owner = stored.data["owner"].as_str().unwrap();
    assert!(
        owner.starts_with("claimant-"),
        "owner should have transferred once, got {:?}",
        owner
    );

    // And the corpus still describes exactly the one document.
    let (docs, tokens) = state
        .db
        .search_corpus_stats(PROJECT, "posts", "search")
        .await
        .unwrap();
    assert_eq!(docs, 1);
    assert_eq!(tokens, TOKENS_PER_DOC);
}

/// With embed-on-write enabled, concurrent patches to one document must leave
/// a vector that describes the text actually stored. The embedding is computed
/// outside the locks so a provider call cannot stall other writers, which
/// means a concurrent write can invalidate it — the write path detects that
/// and re-embeds rather than storing a vector for text it discarded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_patches_leave_the_vector_matching_the_text() {
    use brickbed_server::embed::{EmbeddingProvider, MockProvider};

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
    let embedder: Arc<dyn EmbeddingProvider> = Arc::new(MockProvider::new(4));
    let db = Db::open_with_embedder(&config, Some(embedder))
        .await
        .unwrap();
    let state = Arc::new(AppState {
        db,
        api_keys: config.api_keys.clone(),
        keybroker: None,
        jwks: JwksCache::new(HttpJwksFetcher::new()),
    });
    let app = build_router(state.clone());

    let (status, _) = send(
        &app,
        "PUT",
        &format!("/v1/{}/_schema", PROJECT),
        Some(json!({"collections": {"posts": {
            "fields": {
                "title": {"type": "string"},
                "slug": {"type": "string"},
                "embedding": {"type": "vector", "dims": 4, "from": ["title"], "model": "m"}
            }
        }}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, created) = send(
        &app,
        "POST",
        &format!("/v1/{}/posts", PROJECT),
        Some(json!({"title": "original text", "slug": "target"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = Arc::new(created["_id"].as_str().unwrap().to_string());

    const WRITERS: usize = 8;
    let statuses = concurrently(WRITERS, |i| {
        let app = app.clone();
        let id = id.clone();
        async move {
            send(
                &app,
                "PATCH",
                &format!("/v1/{}/posts/{}", PROJECT, id),
                Some(json!({"title": format!("rewritten text {:02}", i)})),
            )
            .await
            .0
        }
    })
    .await;
    assert!(
        statuses.iter().all(|s| *s == StatusCode::OK),
        "statuses: {:?}",
        statuses
    );

    let (_, stored) = send(&app, "GET", &format!("/v1/{}/posts/{}", PROJECT, id), None).await;
    let title = stored["title"].as_str().unwrap().to_string();
    let vector = stored["embedding"].clone();
    assert!(vector.is_array(), "the document kept a vector");

    // The provider is deterministic, so a fresh document with the surviving
    // title must embed to the same vector. If a patch had stored a vector for
    // text another patch replaced, these would differ.
    let (status, reference) = send(
        &app,
        "POST",
        &format!("/v1/{}/posts", PROJECT),
        Some(json!({"title": title, "slug": "reference"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        vector, reference["embedding"],
        "stored vector describes text the document no longer has"
    );
}

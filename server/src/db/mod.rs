use bytes::Bytes;
use object_store::aws::AmazonS3Builder;
use object_store::local::LocalFileSystem;
use object_store::ObjectStore;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use slatedb::config::WriteOptions;
use slatedb::{Db as SlateDb, WriteBatch};
use std::cmp::Ordering;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use ulid::Ulid;

use crate::config::{Config, StorageBackend};
use crate::embed::{self, EmbeddingProvider};
use crate::error::AppError;
use crate::fts;
use crate::index;
use crate::schema::{CollectionSchema, ProjectSchema, VectorIndexDef};
use crate::validate::{reject_reserved_document_fields, validate_doc};
use crate::vector::{self, VectorSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    #[serde(rename = "_id")]
    pub id: String,
    #[serde(rename = "_createdAt")]
    pub created_at: u64,
    #[serde(rename = "_updatedAt")]
    pub updated_at: u64,
    #[serde(flatten)]
    pub data: Map<String, Value>,
}

/// Key format: {project}:{collection}:{id}
/// Project names cannot start with `_`, so the `_meta:`, `_idx:`, `_fts:` and
/// `_vec:` namespaces never collide with document keys.
fn make_key(project: &str, collection: &str, id: &str) -> Vec<u8> {
    format!("{}:{}:{}", project, collection, id).into_bytes()
}

/// Prefix for listing all docs in a collection
fn make_prefix(project: &str, collection: &str) -> String {
    format!("{}:{}:", project, collection)
}

fn schema_key(project: &str) -> Vec<u8> {
    format!("_meta:{}:schema", project).into_bytes()
}

/// Parse key back to (project, collection, id)
fn parse_key(key: &[u8]) -> Option<(String, String, String)> {
    let s = std::str::from_utf8(key).ok()?;
    let mut parts = s.splitn(3, ':');
    let project = parts.next()?.to_string();
    let collection = parts.next()?.to_string();
    let id = parts.next()?.to_string();
    Some((project, collection, id))
}

/// (project, collection, vector index) -> that index's vectors.
type VecCacheKey = (String, String, String);

/// Reciprocal-rank-fusion constant, from the original paper. Large enough
/// that the top handful of ranks in each arm are worth roughly the same, so
/// one arm's runaway score cannot dominate the other.
const RRF_K: f64 = 60.0;

/// How much deeper than the requested page to retrieve when hits can be
/// dropped after retrieval (post-filtering) or need fusing across two arms.
const OVERFETCH: usize = 4;

/// Everything an authorization decision may rest on, as it stands **inside the
/// write lock** — not as it stood when the request was admitted.
///
/// Both the rules and the document can be changed by a concurrent writer
/// between a caller's check and its write, so both are supplied here rather
/// than captured beforehand. All of it is already in memory at this point: the
/// schema is the one the write itself uses, so evaluating a predicate adds no
/// I/O to the critical section.
pub struct PreconditionCtx<'a> {
    /// Collection schema as stored right now; `None` if the collection is
    /// undeclared.
    pub collection: Option<&'a CollectionSchema>,
    /// The stored document; absent for creates, which replace nothing.
    pub existing: Option<&'a Map<String, Value>>,
    /// The document about to be written; absent for deletes.
    pub next: Option<&'a Map<String, Value>>,
}

/// Authorization predicate evaluated under the write lock. Returning `false`
/// aborts the write with `Forbidden`.
///
/// A caller that reads, decides, and only then writes can be raced: another
/// writer may change the document — or the rule — the decision rested on.
/// Handing the decision down to where the write happens closes that window.
pub type Precondition<'a> = &'a (dyn Fn(PreconditionCtx<'_>) -> bool + Send + Sync);

/// Authorization predicate for a schema push, evaluated under the write lock
/// against the schema currently stored (`None` when the project has none).
pub type SchemaPrecondition<'a> = &'a (dyn Fn(Option<&ProjectSchema>) -> bool + Send + Sync);

/// Per-document write locks are sharded this many ways. Only collisions on the
/// same shard serialise unrelated documents, and 512 keeps that rare while
/// costing one small mutex each.
const DOC_LOCK_SHARDS: usize = 512;

/// How many times a patch will re-embed when a concurrent write changes the
/// text it embedded. Each retry costs a provider call, so the last attempt
/// gives up on staying lock-free and embeds inside the critical section.
const EMBED_RETRIES: usize = 2;

/// Locks a write holds while it builds its batch. They are handed to
/// `commit_write` rather than left to fall out of scope so that it can release
/// them the instant the batch is sequenced — before the durable flush, which
/// no lock may span.
struct WriteGuards<'a> {
    _schema: tokio::sync::RwLockReadGuard<'a, ()>,
    _document: Option<tokio::sync::MutexGuard<'a, ()>>,
}

/// Equality predicate applied to search hits, naming a declared index and the
/// values to bind to a prefix of its fields.
#[derive(Debug, Clone, Copy)]
pub struct Filter<'a> {
    pub index: &'a str,
    pub params: &'a Map<String, Value>,
}

/// One `_search` request. Which of `query`/`vector` is set picks the mode:
/// both means hybrid.
#[derive(Debug, Clone, Copy)]
pub struct SearchParams<'a> {
    pub project: &'a str,
    pub collection: &'a str,
    pub query: Option<&'a str>,
    pub vector: Option<&'a [f32]>,
    /// Search index for the text arm; defaults to the collection's first.
    pub text_index: Option<&'a str>,
    /// Vector index for the vector arm; defaults to the collection's first.
    pub vector_index: Option<&'a str>,
    pub filter: Option<Filter<'a>>,
    pub limit: usize,
}

/// Reciprocal rank fusion: a document scores `1/(k + rank)` in each arm it
/// appears in, 1-based. Only ranks cross the boundary, so BM25 and similarity
/// magnitudes never have to be made commensurable.
mod administration;
mod documents;
mod equality_indexes;
mod indexes;
mod lifecycle;
mod schemas;
mod search;
mod write;

pub struct Db {
    slate: SlateDb,
    /// Excludes document writes while a schema is pushed. `put_schema` drops
    /// and rebuilds every index entry, so a write racing the rebuild could
    /// land unindexed; document writes take this shared and run concurrently.
    schema_lock: RwLock<()>,
    /// Serialises the `_fts` corpus-stats read-modify-write, which SlateDB
    /// cannot do atomically. Held only until the batch is sequenced — never
    /// across the durable flush, or writers could not group-commit.
    stats_lock: Mutex<()>,
    /// Sharded per-document locks. Updating a document reads it, evaluates any
    /// precondition against what it read, and derives which index entries to
    /// retire from that same snapshot; two writers working from one snapshot
    /// would each retire only the original's entries and leave the loser's
    /// behind. Sharding keeps writes to different documents concurrent.
    doc_locks: Vec<Mutex<()>>,
    /// Vectors held in memory for brute-force search, filled by the first
    /// search that needs them and dropped wholesale when the collection is
    /// written to.
    vec_cache: RwLock<HashMap<VecCacheKey, Arc<VectorSet>>>,
    /// Bumped on every invalidation. A search that scanned before a write
    /// commits compares this to decide its snapshot is too old to cache.
    vec_generation: AtomicU64,
    /// Embed-on-write provider. `None` disables server-side embedding.
    embedder: Option<Arc<dyn EmbeddingProvider>>,
}

#[cfg(test)]
mod compatibility_tests {
    use super::*;

    #[test]
    fn document_key_encoding_is_unchanged() {
        assert_eq!(
            make_key("project", "posts", "01JABC"),
            b"project:posts:01JABC"
        );
        assert_eq!(make_prefix("project", "posts"), "project:posts:");
        assert_eq!(schema_key("project"), b"_meta:project:schema");
        assert_eq!(
            parse_key(b"project:posts:id:with:colons"),
            Some((
                "project".to_string(),
                "posts".to_string(),
                "id:with:colons".to_string(),
            ))
        );
    }
}

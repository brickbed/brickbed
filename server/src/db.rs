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
fn fuse_rrf(text: &[(String, f64)], vectors: &[(String, f64)]) -> Vec<(String, f64)> {
    let mut fused: HashMap<&str, f64> = HashMap::new();
    for arm in [text, vectors] {
        for (rank, (id, _)) in arm.iter().enumerate() {
            *fused.entry(id.as_str()).or_default() += 1.0 / (RRF_K + (rank + 1) as f64);
        }
    }

    let mut ranked: Vec<(String, f64)> = fused
        .into_iter()
        .map(|(id, score)| (id.to_string(), score))
        .collect();
    // Ties break on id so the fused order stays deterministic.
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked
}

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

impl Db {
    pub async fn open(config: &Config) -> Result<Self, AppError> {
        let embedder = embed::from_config(config.embeddings.as_ref())?;
        Self::open_with_embedder(config, embedder).await
    }

    /// Open with an explicit embedding provider, bypassing `EMBEDDINGS_*`.
    /// Tests use this to inject a deterministic or failing provider.
    pub async fn open_with_embedder(
        config: &Config,
        embedder: Option<Arc<dyn EmbeddingProvider>>,
    ) -> Result<Self, AppError> {
        let object_store: Arc<dyn ObjectStore> =
            match &config.storage {
                StorageBackend::Local { path } => {
                    std::fs::create_dir_all(path).map_err(|e| {
                        AppError::Internal(format!("Failed to create storage dir: {}", e))
                    })?;
                    Arc::new(LocalFileSystem::new_with_prefix(path).map_err(|e| {
                        AppError::Internal(format!("Failed to init local storage: {}", e))
                    })?)
                }
                StorageBackend::S3 {
                    bucket,
                    endpoint,
                    region,
                    access_key_id,
                    secret_access_key,
                } => {
                    let mut builder = AmazonS3Builder::new()
                        .with_bucket_name(bucket)
                        .with_region(region)
                        .with_access_key_id(access_key_id)
                        .with_secret_access_key(secret_access_key);

                    if let Some(ep) = endpoint {
                        builder = builder.with_endpoint(ep);
                    }

                    Arc::new(builder.build().map_err(|e| {
                        AppError::Internal(format!("Failed to init S3 storage: {}", e))
                    })?)
                }
            };

        let path = object_store::path::Path::from(config.db_path.clone());
        let slate = SlateDb::open(path, object_store)
            .await
            .map_err(|e| AppError::Internal(format!("Failed to open SlateDB: {}", e)))?;

        tracing::info!("SlateDB opened at path: {}", config.db_path);

        Ok(Self {
            slate,
            schema_lock: RwLock::new(()),
            stats_lock: Mutex::new(()),
            doc_locks: (0..DOC_LOCK_SHARDS).map(|_| Mutex::new(())).collect(),
            vec_cache: RwLock::new(HashMap::new()),
            vec_generation: AtomicU64::new(0),
            embedder,
        })
    }

    fn now_millis() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    fn serialize_doc(doc: &Document) -> Result<Bytes, AppError> {
        rmp_serde::to_vec(doc)
            .map(Bytes::from)
            .map_err(|e| AppError::Internal(format!("Serialization error: {}", e)))
    }

    fn deserialize_doc(data: &[u8]) -> Result<Document, AppError> {
        rmp_serde::from_slice(data)
            .map_err(|e| AppError::Internal(format!("Deserialization error: {}", e)))
    }

    async fn write_batch(&self, batch: WriteBatch) -> Result<(), AppError> {
        self.slate
            .write(batch)
            .await
            .map_err(|e| AppError::Internal(format!("DB write error: {}", e)))
    }

    /// Commit a document write, adding the BM25 posting and stats operations
    /// when the collection declares a search index.
    ///
    /// A durable write costs a WAL flush of roughly 90ms. SlateDB coalesces
    /// concurrent writers into one flush — group commit — but only if they are
    /// actually concurrent. The corpus-stats entry is a read-modify-write, so
    /// writers touching it must be serialised; serialising them *across the
    /// flush* meant one document per flush and collapsed throughput by two
    /// orders of magnitude.
    ///
    /// So the lock covers reading the stats, building the batch, and
    /// sequencing it — `await_durable: false` returns once the batch is
    /// applied and its commit sequence published, which is what makes the next
    /// writer's stats read see it — and is released before durability is
    /// awaited. Every writer still waits for its own data to be durable before
    /// the caller is told the write succeeded; they simply wait together.
    ///
    /// Collections without a search index share no such state and take no
    /// stats lock at all. Both paths then force the flush rather than letting
    /// `await_durable` wait for SlateDB's periodic tick: waiting for the tick
    /// costs a serial writer most of a flush interval (~100ms measured), while
    /// forcing it costs ~1ms and still coalesces whatever else is queued.
    #[allow(clippy::too_many_arguments)]
    async fn commit_write(
        &self,
        mut batch: WriteBatch,
        schema: Option<&ProjectSchema>,
        project: &str,
        collection: &str,
        old: Option<&Document>,
        new: Option<&Document>,
        guards: WriteGuards<'_>,
    ) -> Result<(), AppError> {
        let keeps_stats = schema
            .and_then(|s| s.collection(collection))
            .is_some_and(|coll| !coll.search_indexes.is_empty());

        if keeps_stats {
            let _stats = self.stats_lock.lock().await;
            self.apply_fts_ops(&mut batch, schema, project, collection, old, new)
                .await?;
            self.sequence(batch).await?;
        } else {
            self.sequence(batch).await?;
        }

        // The batch is applied and its commit sequence published, so every
        // successor can now see it. Releasing here rather than at end of scope
        // is the point of the whole exercise: holding a lock across the flush
        // is what made writers wait for each other's durability instead of
        // sharing one.
        drop(guards);

        // Sequenced data is readable whether or not the flush succeeds, so the
        // vector cache has to be dropped either way — otherwise a failed flush
        // would leave searches serving vectors the database no longer holds.
        let flushed = self
            .slate
            .flush()
            .await
            .map_err(|e| AppError::Internal(format!("DB flush error: {}", e)));
        self.invalidate_vectors(project, Some(collection)).await;
        flushed
    }

    /// Apply a batch without waiting for durability. It is visible to reads
    /// once this returns, which is what lets the stats lock be released before
    /// the flush.
    async fn sequence(&self, batch: WriteBatch) -> Result<(), AppError> {
        self.slate
            .write_with_options(
                batch,
                &WriteOptions {
                    await_durable: false,
                },
            )
            .await
            .map_err(|e| AppError::Internal(format!("DB write error: {}", e)))
    }

    /// Guard serialising writes to one document. Insert does not need it: its
    /// id is freshly generated, so no other writer can be touching it.
    async fn lock_document(
        &self,
        project: &str,
        collection: &str,
        id: &str,
    ) -> tokio::sync::MutexGuard<'_, ()> {
        let mut hasher = DefaultHasher::new();
        (project, collection, id).hash(&mut hasher);
        let shard = (hasher.finish() % self.doc_locks.len() as u64) as usize;
        self.doc_locks[shard].lock().await
    }

    /// Corpus statistics of a search index as `(documents, total tokens)`.
    /// These back BM25's IDF and length normalisation, and they are the one
    /// piece of shared state the write path has to serialise on, so they are
    /// worth being able to inspect.
    pub async fn search_corpus_stats(
        &self,
        project: &str,
        collection: &str,
        index_name: &str,
    ) -> Result<(u64, u64), AppError> {
        let stats = self.fts_stats(project, collection, index_name).await?;
        Ok((stats.n_docs, stats.total_len))
    }

    // ---- Schema ----

    pub async fn get_schema(&self, project: &str) -> Result<Option<ProjectSchema>, AppError> {
        let value = self
            .slate
            .get(schema_key(project).as_slice())
            .await
            .map_err(|e| AppError::Internal(format!("DB read error: {}", e)))?;

        match value {
            None => Ok(None),
            Some(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| AppError::Internal(format!("Schema decode error: {}", e))),
        }
    }

    /// Store the project schema and rebuild all index, search and vector
    /// entries for existing documents (delete old entries, scan docs,
    /// re-index). Fine at demo scale.
    pub async fn put_schema(&self, project: &str, schema: &ProjectSchema) -> Result<(), AppError> {
        self.put_schema_checked(project, schema, None).await
    }

    /// `put_schema`, with an authorization predicate evaluated against the
    /// *currently stored* schema under the write lock.
    ///
    /// A gate that compares the incoming schema against a separately-read
    /// stored copy can be raced: a concurrent push lands in between, and the
    /// payload that passed the gate then overwrites it. Comparing inside the
    /// critical section is the only way the comparison describes the schema
    /// actually being replaced.
    pub async fn put_schema_checked(
        &self,
        project: &str,
        schema: &ProjectSchema,
        precondition: Option<SchemaPrecondition<'_>>,
    ) -> Result<(), AppError> {
        let encoded = serde_json::to_vec(schema)
            .map_err(|e| AppError::Internal(format!("Schema encode error: {}", e)))?;

        // Exclusive, unlike a document write: the backfill below drops and
        // rebuilds every index entry for the project, so no document write may
        // be in flight beside it or it could land unindexed. This also makes
        // the precondition atomic with the push it authorises.
        let _schema_guard = self.schema_lock.write().await;

        if let Some(allowed) = precondition {
            // One extra read inside a critical section that already scans every
            // index entry and document of the project to rebuild them.
            let stored = self.get_schema(project).await?;
            if !allowed(stored.as_ref()) {
                return Err(AppError::Forbidden);
            }
        }

        let mut batch = WriteBatch::new();
        batch.put(schema_key(project), encoded);

        // Drop all existing index, search and vector entries for the project.
        for prefix in [
            index::project_index_prefix(project),
            fts::project_prefix(project),
            vector::project_prefix(project),
        ] {
            let end = index::prefix_end(&prefix);
            let mut iter = self
                .slate
                .scan(prefix.as_slice()..end.as_slice())
                .await
                .map_err(|e| AppError::Internal(format!("Scan error: {}", e)))?;
            while let Some(entry) = iter
                .next()
                .await
                .map_err(|e| AppError::Internal(format!("Iter error: {}", e)))?
            {
                batch.delete(entry.key.as_ref());
            }
        }

        // Corpus stats are rebuilt from scratch: seed every declared search
        // index so one that lost all its documents resets to zero.
        let mut stats: BTreeMap<(String, String), fts::Stats> = BTreeMap::new();
        for (coll_name, coll_schema) in &schema.collections {
            for sidx in &coll_schema.search_indexes {
                stats.insert(
                    (coll_name.clone(), sidx.name.clone()),
                    fts::Stats::default(),
                );
            }
        }

        // Re-index every document in every collection the schema declares.
        let doc_prefix = format!("{}:", project).into_bytes();
        let doc_end = index::prefix_end(&doc_prefix);
        let mut iter = self
            .slate
            .scan(doc_prefix.as_slice()..doc_end.as_slice())
            .await
            .map_err(|e| AppError::Internal(format!("Scan error: {}", e)))?;
        while let Some(entry) = iter
            .next()
            .await
            .map_err(|e| AppError::Internal(format!("Iter error: {}", e)))?
        {
            let Some((proj, collection, _)) = parse_key(&entry.key) else {
                continue;
            };
            if proj != project {
                continue;
            }
            let Some(coll_schema) = schema.collection(&collection) else {
                continue;
            };
            let doc = Self::deserialize_doc(&entry.value)?;
            for idx in &coll_schema.indexes {
                batch.put(
                    index::entry_key(project, &collection, idx, &doc.data, &doc.id),
                    doc.id.as_bytes(),
                );
            }
            for sidx in &coll_schema.search_indexes {
                let terms = fts::doc_terms(sidx, &doc.data);
                for (term, tf) in &terms.tf {
                    batch.put(
                        fts::posting_key(project, &collection, &sidx.name, term, &doc.id),
                        fts::encode_posting(*tf, terms.len),
                    );
                }
                stats
                    .entry((collection.clone(), sidx.name.clone()))
                    .or_default()
                    .add_doc(terms.len);
            }
            for vidx in &coll_schema.vector_indexes {
                // A document whose field is absent or the wrong width simply
                // stays out of the index (its stale entry was dropped above).
                if let Some(v) = vector::doc_vector(&doc.data, &vidx.field, vidx.dims as usize) {
                    batch.put(
                        vector::entry_key(project, &collection, &vidx.name, &doc.id),
                        vector::encode(&v),
                    );
                }
            }
        }

        for ((collection, index_name), stats) in &stats {
            batch.put(
                fts::stats_key(project, collection, index_name),
                fts::encode_stats(stats)?,
            );
        }

        self.write_batch(batch).await?;
        self.invalidate_vectors(project, None).await;
        Ok(())
    }

    /// Validate against the collection schema (if any) and add index
    /// puts/deletes to the batch alongside a document write.
    fn apply_index_ops(
        batch: &mut WriteBatch,
        schema: Option<&ProjectSchema>,
        project: &str,
        collection: &str,
        old: Option<&Document>,
        new: &Document,
    ) {
        let Some(coll_schema) = schema.and_then(|s| s.collection(collection)) else {
            return;
        };
        for idx in &coll_schema.indexes {
            if let Some(old_doc) = old {
                batch.delete(index::entry_key(
                    project,
                    collection,
                    idx,
                    &old_doc.data,
                    &old_doc.id,
                ));
            }
            batch.put(
                index::entry_key(project, collection, idx, &new.data, &new.id),
                new.id.as_bytes(),
            );
        }
    }

    /// Current corpus stats for a search index (absent = empty index).
    async fn fts_stats(
        &self,
        project: &str,
        collection: &str,
        index_name: &str,
    ) -> Result<fts::Stats, AppError> {
        let key = fts::stats_key(project, collection, index_name);
        match self
            .slate
            .get(key.as_slice())
            .await
            .map_err(|e| AppError::Internal(format!("DB read error: {}", e)))?
        {
            None => Ok(fts::Stats::default()),
            Some(bytes) => fts::decode_stats(&bytes),
        }
    }

    /// Add posting puts/deletes and the stats update for a document write to
    /// the batch. `new` is `None` for deletes. Callers must hold the write
    /// lock: the stats entry is read-modify-write.
    async fn apply_fts_ops(
        &self,
        batch: &mut WriteBatch,
        schema: Option<&ProjectSchema>,
        project: &str,
        collection: &str,
        old: Option<&Document>,
        new: Option<&Document>,
    ) -> Result<(), AppError> {
        let Some(coll_schema) = schema.and_then(|s| s.collection(collection)) else {
            return Ok(());
        };
        for sidx in &coll_schema.search_indexes {
            let mut stats = self.fts_stats(project, collection, &sidx.name).await?;

            // Deletes first: a term the document keeps is re-put below, and the
            // last op for a key wins within a batch.
            if let Some(old_doc) = old {
                let terms = fts::doc_terms(sidx, &old_doc.data);
                for term in terms.tf.keys() {
                    batch.delete(fts::posting_key(
                        project,
                        collection,
                        &sidx.name,
                        term,
                        &old_doc.id,
                    ));
                }
                stats.remove_doc(terms.len);
            }
            if let Some(new_doc) = new {
                let terms = fts::doc_terms(sidx, &new_doc.data);
                for (term, tf) in &terms.tf {
                    batch.put(
                        fts::posting_key(project, collection, &sidx.name, term, &new_doc.id),
                        fts::encode_posting(*tf, terms.len),
                    );
                }
                stats.add_doc(terms.len);
            }

            batch.put(
                fts::stats_key(project, collection, &sidx.name),
                fts::encode_stats(&stats)?,
            );
        }
        Ok(())
    }

    /// Add vector puts/deletes for a document write to the batch. `new` is
    /// `None` for deletes. Entries are keyed by document id alone, so writing
    /// the new vector (or deleting the key when the document no longer has
    /// one) is enough to retire the old value.
    fn apply_vec_ops(
        batch: &mut WriteBatch,
        schema: Option<&ProjectSchema>,
        project: &str,
        collection: &str,
        id: &str,
        new: Option<&Document>,
    ) {
        let Some(coll_schema) = schema.and_then(|s| s.collection(collection)) else {
            return;
        };
        for vidx in &coll_schema.vector_indexes {
            let key = vector::entry_key(project, collection, &vidx.name, id);
            match new.and_then(|doc| vector::doc_vector(&doc.data, &vidx.field, vidx.dims as usize))
            {
                Some(v) => batch.put(key, vector::encode(&v)),
                None => batch.delete(key),
            }
        }
    }

    /// Drop cached vectors: one collection, or the whole project when
    /// `collection` is `None`. Called after the write commits, so a search
    /// racing the write either sees the new state or declines to cache.
    async fn invalidate_vectors(&self, project: &str, collection: Option<&str>) {
        let mut cache = self.vec_cache.write().await;
        cache.retain(|(p, c, _), _| !(p == project && collection.is_none_or(|coll| c == coll)));
        self.vec_generation.fetch_add(1, AtomicOrdering::Release);
    }

    /// Vectors of one index, from cache or by scanning the `_vec` prefix.
    async fn vectors(
        &self,
        project: &str,
        collection: &str,
        index_name: &str,
    ) -> Result<Arc<VectorSet>, AppError> {
        let key = (
            project.to_string(),
            collection.to_string(),
            index_name.to_string(),
        );
        {
            let cache = self.vec_cache.read().await;
            if let Some(hit) = cache.get(&key) {
                return Ok(hit.clone());
            }
        }

        let generation = self.vec_generation.load(AtomicOrdering::Acquire);
        let prefix = vector::index_prefix(project, collection, index_name);
        let end = index::prefix_end(&prefix);

        let mut set = VectorSet::default();
        let mut iter = self
            .slate
            .scan(prefix.as_slice()..end.as_slice())
            .await
            .map_err(|e| AppError::Internal(format!("Scan error: {}", e)))?;
        while let Some(entry) = iter
            .next()
            .await
            .map_err(|e| AppError::Internal(format!("Iter error: {}", e)))?
        {
            let Some(id) = vector::entry_doc_id(&entry.key, &prefix) else {
                continue;
            };
            let Some(v) = vector::decode(&entry.value) else {
                continue;
            };
            set.push(id, v);
        }

        let set = Arc::new(set);
        let mut cache = self.vec_cache.write().await;
        // A write landed while we scanned: serve this snapshot but don't keep it.
        if self.vec_generation.load(AtomicOrdering::Acquire) == generation {
            cache.insert(key, set.clone());
        }
        Ok(set)
    }

    /// Fill server-provided vectors before the document is written. The
    /// provider is called outside the write batch, so a failure aborts the
    /// write with nothing persisted.
    ///
    /// Callers pre-validate against the advisory schema first, so an invalid
    /// document never costs a provider call; the authoritative under-lock
    /// validation then re-checks (incl. the produced vector's shape).
    /// `validate_doc` lets a server-filled vector be absent.
    ///
    /// ACCEPTED RACE (review 2026-08-07): embedding reads the schema's
    /// `from`/`model` config outside the write lock, so a write racing a
    /// schema push may embed under the old config. This matches system
    /// semantics — a config change never re-embeds stored documents either —
    /// so the raced write is indistinguishable from the existing corpus.
    /// Text↔vector consistency is still guaranteed (the patch retry loop
    /// compares source FIELDS under the lock). Config migration / re-embed
    /// is the backlog item that closes both at once.
    async fn apply_embeddings(
        &self,
        schema: Option<&ProjectSchema>,
        collection: &str,
        data: &mut Map<String, Value>,
        patch: Option<embed::PatchContext<'_>>,
    ) -> Result<bool, AppError> {
        let Some(embedder) = self.embedder.as_ref() else {
            return Ok(false);
        };
        let Some(coll_schema) = schema.and_then(|s| s.collection(collection)) else {
            return Ok(false);
        };

        let plan = embed::plan(coll_schema, data, patch);
        if plan.is_empty() {
            return Ok(false);
        }

        // Sources emptied: whatever vector is stored describes text the
        // document no longer has, so drop it and let it leave the index.
        for field in &plan.clear {
            data.remove(field);
        }

        // One request per model: a collection may embed several fields, and
        // batching keeps that to a single round trip per model.
        let mut by_model: BTreeMap<&str, Vec<&embed::Pending>> = BTreeMap::new();
        for item in &plan.embed {
            by_model.entry(item.model.as_str()).or_default().push(item);
        }

        for (model, group) in by_model {
            let texts: Vec<String> = group.iter().map(|item| item.text.clone()).collect();
            let vectors = embedder.embed(&texts, model).await?;
            if vectors.len() != group.len() {
                return Err(AppError::Embedding(format!(
                    "{} returned {} vectors for {} inputs",
                    embedder.name(),
                    vectors.len(),
                    group.len()
                )));
            }
            for (item, vector) in group.iter().zip(vectors) {
                let value = embed::vector_value(embedder.name(), model, item, vector)?;
                data.insert(item.field.clone(), value);
            }
        }
        Ok(true)
    }

    fn check_schema(
        schema: Option<&ProjectSchema>,
        collection: &str,
        data: &Map<String, Value>,
    ) -> Result<(), AppError> {
        if let Some(coll_schema) = schema.and_then(|s| s.collection(collection)) {
            validate_doc(coll_schema, data)?;
        }
        Ok(())
    }

    // ---- Documents ----

    pub async fn insert(
        &self,
        project: &str,
        collection: &str,
        data: Map<String, Value>,
    ) -> Result<Document, AppError> {
        self.insert_checked(project, collection, data, None).await
    }

    /// `insert`, with an authorization predicate evaluated under the write lock.
    /// There is no stored document to race here, but the *rule* can still be
    /// tightened by a concurrent schema push after the request was admitted.
    pub async fn insert_checked(
        &self,
        project: &str,
        collection: &str,
        data: Map<String, Value>,
        precondition: Option<Precondition<'_>>,
    ) -> Result<Document, AppError> {
        reject_reserved_document_fields(&data)?;
        let mut data = data;

        // Embedding calls an upstream provider and can take seconds, so it
        // happens before any lock is taken: holding one across it would stall
        // every schema push, and every writer sharing this document's shard,
        // for the length of an HTTP round trip.
        //
        // The check below is an advisory copy of the authoritative one further
        // down. It exists so a write that is going to be refused does not pay
        // for a provider call first; the decision that actually governs the
        // write is the one taken under the lock. A rule changing from refuse
        // to allow in between turns into a spurious refusal the caller can
        // retry, never into an unauthorised write.
        let advisory = self.get_schema(project).await?;
        if let Some(allowed) = precondition {
            if !allowed(PreconditionCtx {
                collection: advisory.as_ref().and_then(|s| s.collection(collection)),
                existing: None,
                next: Some(&data),
            }) {
                return Err(AppError::Forbidden);
            }
        }
        // Advisory pre-validation: an invalid document must never cost a
        // provider call. The authoritative check still runs under the lock.
        Self::check_schema(advisory.as_ref(), collection, &data)?;
        self.apply_embeddings(advisory.as_ref(), collection, &mut data, None)
            .await?;

        let schema_guard = self.schema_lock.read().await;
        let schema = self.get_schema(project).await?;

        // Authoritative: evaluated against the schema this write itself uses.
        if let Some(allowed) = precondition {
            if !allowed(PreconditionCtx {
                collection: schema.as_ref().and_then(|s| s.collection(collection)),
                existing: None,
                next: Some(&data),
            }) {
                return Err(AppError::Forbidden);
            }
        }

        // Validation runs under the lock, so a vector embedded against a
        // schema that changed in the window is rejected here rather than
        // stored at the wrong width.
        Self::check_schema(schema.as_ref(), collection, &data)?;

        let now = Self::now_millis();
        let id = Ulid::new().to_string();

        let doc = Document {
            id: id.clone(),
            created_at: now,
            updated_at: now,
            data,
        };

        let mut batch = WriteBatch::new();
        batch.put(
            make_key(project, collection, &id),
            Self::serialize_doc(&doc)?,
        );
        Self::apply_index_ops(&mut batch, schema.as_ref(), project, collection, None, &doc);

        Self::apply_vec_ops(
            &mut batch,
            schema.as_ref(),
            project,
            collection,
            &id,
            Some(&doc),
        );
        self.commit_write(
            batch,
            schema.as_ref(),
            project,
            collection,
            None,
            Some(&doc),
            WriteGuards {
                _schema: schema_guard,
                _document: None,
            },
        )
        .await?;

        Ok(doc)
    }

    pub async fn get(
        &self,
        project: &str,
        collection: &str,
        id: &str,
    ) -> Result<Document, AppError> {
        let key = make_key(project, collection, id);

        let value = self
            .slate
            .get(&key)
            .await
            .map_err(|e| AppError::Internal(format!("DB read error: {}", e)))?
            .ok_or(AppError::NotFound)?;

        Self::deserialize_doc(&value)
    }

    pub async fn replace(
        &self,
        project: &str,
        collection: &str,
        id: &str,
        data: Map<String, Value>,
    ) -> Result<Document, AppError> {
        self.replace_checked(project, collection, id, data, None)
            .await
    }

    /// `replace`, with an authorization predicate evaluated against the stored
    /// document under the write lock.
    pub async fn replace_checked(
        &self,
        project: &str,
        collection: &str,
        id: &str,
        data: Map<String, Value>,
        precondition: Option<Precondition<'_>>,
    ) -> Result<Document, AppError> {
        reject_reserved_document_fields(&data)?;
        let mut data = data;

        // Unlocked: embedding calls a provider, so no lock may be held across
        // it. A replace embeds only the incoming document, never the stored
        // one, so nothing here depends on the snapshot the write later uses.
        // The check is the advisory copy — see `insert_checked`; it spares a
        // refused write the provider call, and the authoritative decision is
        // taken under the lock below.
        let advisory = self.get_schema(project).await?;
        if let Some(allowed) = precondition {
            let stored = self.get(project, collection, id).await?;
            if !allowed(PreconditionCtx {
                collection: advisory.as_ref().and_then(|s| s.collection(collection)),
                existing: Some(&stored.data),
                next: Some(&data),
            }) {
                return Err(AppError::Forbidden);
            }
        }
        // Advisory pre-validation: an invalid document must never cost a
        // provider call. The authoritative check still runs under the lock.
        Self::check_schema(advisory.as_ref(), collection, &data)?;
        self.apply_embeddings(advisory.as_ref(), collection, &mut data, None)
            .await?;

        let schema_guard = self.schema_lock.read().await;
        let schema = self.get_schema(project).await?;

        // Get existing to preserve created_at. The shard guard is taken first:
        // the precondition below, the index deltas and the write must all rest
        // on the same snapshot of this document.
        let doc_guard = self.lock_document(project, collection, id).await;
        let existing = self.get(project, collection, id).await?;

        // Authorize before validating: a caller who may not write this document
        // should not learn which of its fields are malformed. This is the
        // decision that governs the write.
        if let Some(allowed) = precondition {
            if !allowed(PreconditionCtx {
                collection: schema.as_ref().and_then(|s| s.collection(collection)),
                existing: Some(&existing.data),
                next: Some(&data),
            }) {
                return Err(AppError::Forbidden);
            }
        }

        Self::check_schema(schema.as_ref(), collection, &data)?;
        let now = Self::now_millis();

        let doc = Document {
            id: id.to_string(),
            created_at: existing.created_at,
            updated_at: now,
            data,
        };

        let mut batch = WriteBatch::new();
        batch.put(
            make_key(project, collection, id),
            Self::serialize_doc(&doc)?,
        );
        Self::apply_index_ops(
            &mut batch,
            schema.as_ref(),
            project,
            collection,
            Some(&existing),
            &doc,
        );

        Self::apply_vec_ops(
            &mut batch,
            schema.as_ref(),
            project,
            collection,
            id,
            Some(&doc),
        );
        self.commit_write(
            batch,
            schema.as_ref(),
            project,
            collection,
            Some(&existing),
            Some(&doc),
            WriteGuards {
                _schema: schema_guard,
                _document: Some(doc_guard),
            },
        )
        .await?;

        Ok(doc)
    }

    pub async fn patch(
        &self,
        project: &str,
        collection: &str,
        id: &str,
        updates: Map<String, Value>,
    ) -> Result<Document, AppError> {
        self.patch_checked(project, collection, id, updates, None)
            .await
    }

    /// `patch`, with an authorization predicate evaluated against the stored
    /// document *and the document the merge would produce*, under the write
    /// lock. The merge happens here, so this is the only place both sides are
    /// known to be the ones actually being written.
    pub async fn patch_checked(
        &self,
        project: &str,
        collection: &str,
        id: &str,
        updates: Map<String, Value>,
        precondition: Option<Precondition<'_>>,
    ) -> Result<Document, AppError> {
        reject_reserved_document_fields(&updates)?;
        // A patch is the one write whose embedding input depends on the stored
        // document, so it cannot simply embed before locking: the text it
        // embeds has to be the text it stores. It embeds from an unlocked read
        // and then, under the lock, checks whether the document moved. If it
        // did, the vector describes text that is no longer there and the work
        // is redone. Same-document patches are rare, so this almost never
        // loops; the final attempt embeds under the lock instead of spinning.
        for attempt in 0..=EMBED_RETRIES {
            let embed_under_lock = attempt == EMBED_RETRIES;

            let advisory = self.get_schema(project).await?;
            let early = self.get(project, collection, id).await?;
            let mut merged = early.data.clone();
            for (k, v) in &updates {
                merged.insert(k.clone(), v.clone());
            }

            // Advisory copy of the decision below: spares a refused write a
            // provider call. The authoritative one is taken under the lock.
            if let Some(allowed) = precondition {
                if !allowed(PreconditionCtx {
                    collection: advisory.as_ref().and_then(|s| s.collection(collection)),
                    existing: Some(&early.data),
                    next: Some(&merged),
                }) {
                    return Err(AppError::Forbidden);
                }
            }

            let embedded = if embed_under_lock {
                false
            } else {
                self.apply_embeddings(
                    advisory.as_ref(),
                    collection,
                    &mut merged,
                    Some(embed::PatchContext {
                        previous: &early.data,
                        updates: &updates,
                    }),
                )
                .await?
            };

            let schema_guard = self.schema_lock.read().await;
            let schema = self.get_schema(project).await?;
            let doc_guard = self.lock_document(project, collection, id).await;
            let existing = self.get(project, collection, id).await?;

            // Only a vector computed from text that has since changed is
            // wasted; a patch that embedded nothing does not care what moved.
            if embedded && existing.data != early.data {
                drop(doc_guard);
                drop(schema_guard);
                continue;
            }

            let now = Self::now_millis();
            let mut doc = existing.clone();
            if embedded {
                // `merged` is this same merge plus the vector, since the
                // document did not change under us.
                doc.data = merged;
            } else {
                for (k, v) in &updates {
                    doc.data.insert(k.clone(), v.clone());
                }
            }
            doc.updated_at = now;

            if let Some(allowed) = precondition {
                if !allowed(PreconditionCtx {
                    collection: schema.as_ref().and_then(|s| s.collection(collection)),
                    existing: Some(&existing.data),
                    next: Some(&doc.data),
                }) {
                    return Err(AppError::Forbidden);
                }
            }

            // Validate the merged document, not just the patch.
            Self::check_schema(schema.as_ref(), collection, &doc.data)?;

            if embed_under_lock {
                // Last attempt: correctness over concurrency.
                self.apply_embeddings(
                    schema.as_ref(),
                    collection,
                    &mut doc.data,
                    Some(embed::PatchContext {
                        previous: &existing.data,
                        updates: &updates,
                    }),
                )
                .await?;
            }

            let mut batch = WriteBatch::new();
            batch.put(
                make_key(project, collection, id),
                Self::serialize_doc(&doc)?,
            );
            Self::apply_index_ops(
                &mut batch,
                schema.as_ref(),
                project,
                collection,
                Some(&existing),
                &doc,
            );
            Self::apply_vec_ops(
                &mut batch,
                schema.as_ref(),
                project,
                collection,
                id,
                Some(&doc),
            );
            self.commit_write(
                batch,
                schema.as_ref(),
                project,
                collection,
                Some(&existing),
                Some(&doc),
                WriteGuards {
                    _schema: schema_guard,
                    _document: Some(doc_guard),
                },
            )
            .await?;

            return Ok(doc);
        }
        unreachable!("the final attempt embeds under the lock and returns")
    }

    pub async fn delete(&self, project: &str, collection: &str, id: &str) -> Result<(), AppError> {
        self.delete_checked(project, collection, id, None).await
    }

    /// `delete`, with an authorization predicate evaluated against the stored
    /// document under the write lock. `next` is `None`: a delete stores nothing.
    pub async fn delete_checked(
        &self,
        project: &str,
        collection: &str,
        id: &str,
        precondition: Option<Precondition<'_>>,
    ) -> Result<(), AppError> {
        let schema_guard = self.schema_lock.read().await;

        let schema = self.get_schema(project).await?;
        let doc_guard = self.lock_document(project, collection, id).await;
        let existing = self.get(project, collection, id).await?;

        if let Some(allowed) = precondition {
            if !allowed(PreconditionCtx {
                collection: schema.as_ref().and_then(|s| s.collection(collection)),
                existing: Some(&existing.data),
                next: None,
            }) {
                return Err(AppError::Forbidden);
            }
        }

        let mut batch = WriteBatch::new();
        batch.delete(make_key(project, collection, id));
        if let Some(coll_schema) = schema.as_ref().and_then(|s| s.collection(collection)) {
            for idx in &coll_schema.indexes {
                batch.delete(index::entry_key(
                    project,
                    collection,
                    idx,
                    &existing.data,
                    &existing.id,
                ));
            }
        }

        Self::apply_vec_ops(&mut batch, schema.as_ref(), project, collection, id, None);
        self.commit_write(
            batch,
            schema.as_ref(),
            project,
            collection,
            Some(&existing),
            None,
            WriteGuards {
                _schema: schema_guard,
                _document: Some(doc_guard),
            },
        )
        .await
    }

    pub async fn list(
        &self,
        project: &str,
        collection: &str,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<(Vec<Document>, Option<String>), AppError> {
        let prefix = make_prefix(project, collection);

        // Determine start key
        let start_key: Vec<u8> = match cursor {
            Some(c) => {
                // Start after the cursor
                let mut k = make_key(project, collection, c);
                // Append a byte to start after this key
                k.push(0x00);
                k
            }
            None => prefix.as_bytes().to_vec(),
        };

        let end_key = index::prefix_end(prefix.as_bytes());

        let mut docs = Vec::new();
        let mut iter = self
            .slate
            .scan(start_key.as_slice()..end_key.as_slice())
            .await
            .map_err(|e| AppError::Internal(format!("Scan error: {}", e)))?;

        while let Some(entry) = iter
            .next()
            .await
            .map_err(|e| AppError::Internal(format!("Iter error: {}", e)))?
        {
            // Verify key belongs to this project and collection
            if let Some((proj, coll, _)) = parse_key(&entry.key) {
                if proj == project && coll == collection {
                    let doc = Self::deserialize_doc(&entry.value)?;
                    docs.push(doc);
                    if docs.len() > limit {
                        break;
                    }
                }
            }
        }

        let next_cursor = if docs.len() > limit {
            let _ = docs.pop();
            docs.last().map(|d| d.id.clone())
        } else {
            None
        };

        Ok((docs, next_cursor))
    }

    /// Equality/prefix query over a declared index. `cursor` is the hex of the
    /// last-seen index entry key.
    pub async fn query(
        &self,
        project: &str,
        collection: &str,
        index_name: &str,
        params: &Map<String, Value>,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<(Vec<Document>, Option<String>), AppError> {
        let schema = self
            .get_schema(project)
            .await?
            .ok_or_else(|| AppError::BadRequest("no schema pushed for project".to_string()))?;
        let coll_schema = schema.collection(collection).ok_or_else(|| {
            AppError::BadRequest(format!("collection {:?} not in schema", collection))
        })?;
        let idx = coll_schema.index(index_name).ok_or_else(|| {
            AppError::BadRequest(format!(
                "unknown index {:?} on {:?}",
                index_name, collection
            ))
        })?;

        let prefix =
            index::query_prefix(project, collection, idx, params).map_err(AppError::BadRequest)?;
        let end = index::prefix_end(&prefix);

        let start: Vec<u8> = match cursor {
            Some(c) => {
                let mut k = index::decode_cursor(c)
                    .ok_or_else(|| AppError::BadRequest("invalid cursor".to_string()))?;
                if !k.starts_with(&prefix) {
                    return Err(AppError::BadRequest(
                        "cursor does not match query".to_string(),
                    ));
                }
                k.push(0x00);
                k
            }
            None => prefix.clone(),
        };

        // Collect limit+1 entries to detect whether more remain.
        let mut entries: Vec<(Vec<u8>, String)> = Vec::new();
        let mut iter = self
            .slate
            .scan(start.as_slice()..end.as_slice())
            .await
            .map_err(|e| AppError::Internal(format!("Scan error: {}", e)))?;

        while let Some(entry) = iter
            .next()
            .await
            .map_err(|e| AppError::Internal(format!("Iter error: {}", e)))?
        {
            let id = String::from_utf8_lossy(&entry.value).to_string();
            entries.push((entry.key.to_vec(), id));
            if entries.len() > limit {
                break;
            }
        }

        let next_cursor = if entries.len() > limit {
            let _ = entries.pop();
            entries.last().map(|(k, _)| index::encode_cursor(k))
        } else {
            None
        };

        let ids: Vec<String> = entries.into_iter().map(|(_, id)| id).collect();
        let mut docs = Vec::with_capacity(ids.len());
        for id in &ids {
            match self.get(project, collection, id).await {
                Ok(doc) => docs.push(doc),
                // Index entry pointing at a deleted doc: skip (shouldn't
                // happen with batched writes, but don't 500 a whole query).
                Err(AppError::NotFound) => continue,
                Err(e) => return Err(e),
            }
        }

        Ok((docs, next_cursor))
    }

    /// BM25 ranking over a declared search index, as `(id, score)` best first.
    /// `depth` bounds how far down the ranking the caller wants to look.
    async fn text_ranked(
        &self,
        project: &str,
        collection: &str,
        coll_schema: &CollectionSchema,
        index_name: Option<&str>,
        query: &str,
        depth: usize,
    ) -> Result<Vec<(String, f64)>, AppError> {
        let sidx = match index_name {
            Some(name) => coll_schema.search_index(name).ok_or_else(|| {
                AppError::BadRequest(format!(
                    "unknown search index {:?} on {:?}",
                    name, collection
                ))
            })?,
            None => coll_schema.search_indexes.first().ok_or_else(|| {
                AppError::BadRequest(format!("collection {:?} has no search index", collection))
            })?,
        };

        let terms = fts::query_terms(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }

        let stats = self.fts_stats(project, collection, &sidx.name).await?;
        if stats.n_docs == 0 {
            return Ok(Vec::new());
        }
        let avg_len = stats.avg_len();

        let mut scores: HashMap<String, f64> = HashMap::new();
        for term in &terms {
            let prefix = fts::term_prefix(project, collection, &sidx.name, term);
            let end = index::prefix_end(&prefix);

            // Buffer the postings: document frequency (and so IDF) is only
            // known once the term's whole posting list has been scanned.
            let mut postings: Vec<(String, u32, u32)> = Vec::new();
            let mut iter = self
                .slate
                .scan(prefix.as_slice()..end.as_slice())
                .await
                .map_err(|e| AppError::Internal(format!("Scan error: {}", e)))?;
            while let Some(entry) = iter
                .next()
                .await
                .map_err(|e| AppError::Internal(format!("Iter error: {}", e)))?
            {
                let Some(id) = fts::posting_doc_id(&entry.key, &prefix) else {
                    continue;
                };
                let Some((tf, doc_len)) = fts::decode_posting(&entry.value) else {
                    continue;
                };
                postings.push((id, tf, doc_len));
            }

            let idf = fts::idf(stats.n_docs, postings.len() as u64);
            for (id, tf, doc_len) in postings {
                *scores.entry(id).or_default() += fts::bm25_term_score(idf, tf, doc_len, avg_len);
            }
        }

        let mut ranked: Vec<(String, f64)> = scores.into_iter().collect();
        // Ties break on id so pagination-free top-k stays deterministic.
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        ranked.truncate(depth);

        Ok(ranked)
    }

    /// Brute-force nearest-neighbour ranking over a declared vector index, as
    /// `(id, score)` most-similar first, alongside the index it resolved to.
    async fn vector_ranked<'a>(
        &self,
        project: &str,
        collection: &str,
        coll_schema: &'a CollectionSchema,
        index_name: Option<&str>,
        query: &[f32],
        depth: usize,
    ) -> Result<(&'a VectorIndexDef, Vec<(String, f64)>), AppError> {
        let vidx = match index_name {
            Some(name) => coll_schema.vector_index(name).ok_or_else(|| {
                AppError::BadRequest(format!(
                    "unknown vector index {:?} on {:?}",
                    name, collection
                ))
            })?,
            None => coll_schema.vector_indexes.first().ok_or_else(|| {
                AppError::BadRequest(format!("collection {:?} has no vector index", collection))
            })?,
        };

        if query.len() != vidx.dims as usize {
            return Err(AppError::BadRequest(format!(
                "vector has {} dimensions, index {:?} expects {}",
                query.len(),
                vidx.name,
                vidx.dims
            )));
        }

        let set = self.vectors(project, collection, &vidx.name).await?;
        let ranked = vector::top_k(vidx.metric, query, &set.vectors, depth)
            .into_iter()
            .map(|(position, score)| (set.ids[position].clone(), score))
            .collect();

        Ok((vidx, ranked))
    }

    /// Text, vector or hybrid search, best-scoring first. The mode follows
    /// from which of `query`/`vector` the caller supplies; supplying both
    /// fuses the two rankings with RRF.
    pub async fn search(&self, params: SearchParams<'_>) -> Result<Vec<(Document, f64)>, AppError> {
        let SearchParams {
            project,
            collection,
            query,
            vector: query_vector,
            text_index,
            vector_index,
            filter,
            limit,
        } = params;

        let schema = self
            .get_schema(project)
            .await?
            .ok_or_else(|| AppError::BadRequest("no schema pushed for project".to_string()))?;
        let coll_schema = schema.collection(collection).ok_or_else(|| {
            AppError::BadRequest(format!("collection {:?} not in schema", collection))
        })?;

        // Resolve the filter before retrieving: one naming an index that does
        // not exist is a client error, not an empty result set.
        let filter = match filter {
            Some(f) => {
                let idx = coll_schema.index(f.index).ok_or_else(|| {
                    AppError::BadRequest(format!(
                        "unknown filter index {:?} on {:?}",
                        f.index, collection
                    ))
                })?;
                let bound = index::bound_fields(idx, f.params).map_err(AppError::BadRequest)?;
                Some((bound, f.params))
            }
            None => None,
        };

        let hybrid = query.is_some() && query_vector.is_some();
        // Post-filtering drops hits after retrieval, and fusion rewards
        // documents ranked well in both arms, so both need candidates from
        // deeper than the requested page.
        let depth = if hybrid || filter.is_some() {
            limit.saturating_mul(OVERFETCH)
        } else {
            limit
        };

        // Vector-only hits are re-scored from the document that comes back,
        // so the `_score` returned always describes the document returned
        // even if the candidate set lagged a concurrent write. Fused scores
        // are rank-based, so hybrid has nothing to re-score.
        let mut rescore = None;
        let ranked = match (query, query_vector) {
            (Some(text), None) => {
                self.text_ranked(project, collection, coll_schema, text_index, text, depth)
                    .await?
            }
            (None, Some(vec_query)) => {
                let (vidx, ranked) = self
                    .vector_ranked(
                        project,
                        collection,
                        coll_schema,
                        vector_index,
                        vec_query,
                        depth,
                    )
                    .await?;
                rescore = Some((vidx, vec_query));
                ranked
            }
            (Some(text), Some(vec_query)) => {
                let text = self
                    .text_ranked(project, collection, coll_schema, text_index, text, depth)
                    .await?;
                let (_, vectors) = self
                    .vector_ranked(
                        project,
                        collection,
                        coll_schema,
                        vector_index,
                        vec_query,
                        depth,
                    )
                    .await?;
                fuse_rrf(&text, &vectors)
            }
            (None, None) => {
                return Err(AppError::BadRequest(
                    "search requires \"query\" or \"vector\"".to_string(),
                ))
            }
        };

        let mut docs: Vec<(Document, f64)> = Vec::with_capacity(limit.min(ranked.len()));
        for (id, score) in ranked {
            if docs.len() >= limit {
                break;
            }
            let doc = match self.get(project, collection, &id).await {
                Ok(doc) => doc,
                // Index entry pointing at a deleted doc: skip rather than 500.
                Err(AppError::NotFound) => continue,
                Err(e) => return Err(e),
            };
            if let Some((bound, params)) = &filter {
                if !index::matches_params(bound, params, &doc.data) {
                    continue;
                }
            }
            let score = match rescore {
                Some((vidx, vec_query)) => {
                    match vector::doc_vector(&doc.data, &vidx.field, vidx.dims as usize) {
                        Some(v) => vector::similarity(vidx.metric, vec_query, &v),
                        // Vector field removed since the scan: drop the hit.
                        None => continue,
                    }
                }
                None => score,
            };
            docs.push((doc, score));
        }

        if rescore.is_some() {
            docs.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(Ordering::Equal)
                    .then_with(|| a.0.id.cmp(&b.0.id))
            });
        }

        Ok(docs)
    }

    pub async fn close(self) -> Result<(), AppError> {
        self.slate
            .close()
            .await
            .map_err(|e| AppError::Internal(format!("Failed to close DB: {}", e)))
    }

    /// Perform a real engine read for readiness checks. In particular, a
    /// SlateDB writer that has been fenced by a newer client returns an error
    /// here instead of continuing to advertise itself as ready.
    pub async fn health_check(&self) -> Result<(), AppError> {
        self.slate
            .get(b"_meta:health")
            .await
            .map(|_| ())
            .map_err(|e| AppError::Internal(format!("Database health check failed: {}", e)))
    }
}

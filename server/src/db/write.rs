//! Cross-domain atomic sequencing, lock ownership, and durability.

use super::*;

impl Db {
    pub(super) fn now_millis() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    pub(super) fn serialize_doc(doc: &Document) -> Result<Bytes, AppError> {
        rmp_serde::to_vec(doc)
            .map(Bytes::from)
            .map_err(|e| AppError::Internal(format!("Serialization error: {}", e)))
    }

    pub(super) fn deserialize_doc(data: &[u8]) -> Result<Document, AppError> {
        rmp_serde::from_slice(data)
            .map_err(|e| AppError::Internal(format!("Deserialization error: {}", e)))
    }

    pub(super) async fn write_batch(&self, batch: WriteBatch) -> Result<(), AppError> {
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
    pub(super) async fn commit_write(
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
    pub(super) async fn lock_document(
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
}

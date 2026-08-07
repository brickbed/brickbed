//! Equality, full-text, vector, embedding, and cache batch helpers.

use super::*;

impl Db {
    /// Validate against the collection schema (if any) and add index
    /// puts/deletes to the batch alongside a document write.
    pub(super) fn apply_index_ops(
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
    pub(super) async fn fts_stats(
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
    pub(super) async fn apply_fts_ops(
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
    pub(super) fn apply_vec_ops(
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
    pub(super) async fn invalidate_vectors(&self, project: &str, collection: Option<&str>) {
        let mut cache = self.vec_cache.write().await;
        cache.retain(|(p, c, _), _| !(p == project && collection.is_none_or(|coll| c == coll)));
        self.vec_generation.fetch_add(1, AtomicOrdering::Release);
    }

    /// Vectors of one index, from cache or by scanning the `_vec` prefix.
    pub(super) async fn vectors(
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
    pub(super) async fn apply_embeddings(
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
}

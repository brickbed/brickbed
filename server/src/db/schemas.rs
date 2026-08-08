//! Project schema persistence and full index rebuilds.

use super::*;

impl Db {
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
}

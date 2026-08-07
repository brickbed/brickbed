//! Document mutation orchestration and validation.

use super::*;

impl Db {
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
}

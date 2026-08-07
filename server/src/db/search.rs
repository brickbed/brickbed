//! Full-text, vector, and hybrid search orchestration.

use super::*;

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

impl Db {
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
                AppError::Schema(format!(
                    "unknown search index {:?} on {:?}",
                    name, collection
                ))
            })?,
            None => coll_schema.search_indexes.first().ok_or_else(|| {
                AppError::Schema(format!("collection {:?} has no search index", collection))
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
                AppError::Schema(format!(
                    "unknown vector index {:?} on {:?}",
                    name, collection
                ))
            })?,
            None => coll_schema.vector_indexes.first().ok_or_else(|| {
                AppError::Schema(format!("collection {:?} has no vector index", collection))
            })?,
        };

        if query.len() != vidx.dims as usize {
            return Err(AppError::Validation(format!(
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

        let schema = self.get_schema(project).await?.ok_or_else(|| {
            AppError::Schema(
                "no schema pushed for project; push a schema before searching".to_string(),
            )
        })?;
        let coll_schema = schema.collection(collection).ok_or_else(|| {
            AppError::Schema(format!(
                "collection {:?} is not declared in the project schema",
                collection
            ))
        })?;

        // Resolve the filter before retrieving: one naming an index that does
        // not exist is a client error, not an empty result set.
        let filter = match filter {
            Some(f) => {
                let idx = coll_schema.index(f.index).ok_or_else(|| {
                    AppError::Schema(format!(
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
}

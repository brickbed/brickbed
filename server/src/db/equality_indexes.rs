//! Collection scans and declared equality-index queries.

use super::*;

impl Db {
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
}

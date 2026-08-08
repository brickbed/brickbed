//! Database construction and writer lifecycle.

use super::*;

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
}

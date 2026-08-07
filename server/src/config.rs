use std::collections::HashMap;
use std::env;

use crate::auth::parse_api_keys;
use crate::keybroker::{KeyBroker, DEFAULT_INSTANCE_NAME};

#[derive(Debug, Clone)]
pub enum StorageBackend {
    /// Local filesystem storage (for development)
    Local { path: String },
    /// S3-compatible storage (R2, S3, MinIO, etc.)
    S3 {
        bucket: String,
        endpoint: Option<String>,
        region: String,
        access_key_id: String,
        secret_access_key: String,
    },
}

/// Embedding provider selected by `EMBEDDINGS_PROVIDER`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    OpenAi,
    Cohere,
    /// Deterministic, offline. For local development and tests only.
    Mock {
        dims: usize,
    },
}

#[derive(Debug, Clone)]
pub struct EmbeddingsConfig {
    pub provider: ProviderKind,
    pub api_key: String,
    /// Overrides the provider's default endpoint.
    pub base_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub storage: StorageBackend,
    pub db_path: String,
    /// API key -> granted project ("*" grants all projects).
    pub api_keys: HashMap<String, String>,
    /// Present when `INSTANCE_SECRET` is set. Keys derived from it are accepted
    /// in addition to `api_keys`; absent, auth behaves exactly as before.
    pub keybroker: Option<KeyBroker>,
    /// Embed-on-write settings. `None` disables it: documents that do not
    /// carry their own vector simply are not vector-indexed.
    pub embeddings: Option<EmbeddingsConfig>,
}

impl EmbeddingsConfig {
    /// Read the provider named by `EMBEDDINGS_PROVIDER`. Panics on a bad
    /// setting, like the other required configuration: a server that silently
    /// disabled embedding would leave documents quietly unindexed.
    fn from_env(provider: &str) -> Self {
        let (provider, key_var) = match provider.trim().to_lowercase().as_str() {
            "openai" => (ProviderKind::OpenAi, Some("OPENAI_API_KEY")),
            "cohere" => (ProviderKind::Cohere, Some("COHERE_API_KEY")),
            "mock" => {
                let dims = env::var("EMBEDDINGS_MOCK_DIMS")
                    .ok()
                    .and_then(|d| d.parse().ok())
                    .unwrap_or(8);
                tracing::warn!("EMBEDDINGS_PROVIDER=mock: embeddings are fake, not for production");
                (ProviderKind::Mock { dims }, None)
            }
            other => panic!(
                "EMBEDDINGS_PROVIDER {:?} is not one of: openai, cohere, mock",
                other
            ),
        };

        let api_key = match key_var {
            Some(var) => env::var(var)
                .unwrap_or_else(|_| panic!("{} required when EMBEDDINGS_PROVIDER is set", var)),
            None => String::new(),
        };

        Self {
            provider,
            api_key,
            base_url: env::var("EMBEDDINGS_BASE_URL").ok(),
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        let storage = if let Ok(bucket) = env::var("S3_BUCKET") {
            StorageBackend::S3 {
                bucket,
                endpoint: env::var("S3_ENDPOINT").ok(),
                region: env::var("S3_REGION").unwrap_or_else(|_| "auto".to_string()),
                access_key_id: env::var("S3_ACCESS_KEY_ID")
                    .expect("S3_ACCESS_KEY_ID required when S3_BUCKET is set"),
                secret_access_key: env::var("S3_SECRET_ACCESS_KEY")
                    .expect("S3_SECRET_ACCESS_KEY required when S3_BUCKET is set"),
            }
        } else {
            StorageBackend::Local {
                path: env::var("STORAGE_PATH").unwrap_or_else(|_| "./data".to_string()),
            }
        };

        let api_keys = match env::var("API_KEYS") {
            Ok(raw) => {
                let keys = parse_api_keys(&raw);
                if keys.is_empty() {
                    panic!("API_KEYS is set but contains no valid key:project pairs");
                }
                keys
            }
            Err(_) => {
                tracing::warn!("API_KEYS not set; using dev default dev-key:demo");
                HashMap::from([("dev-key".to_string(), "demo".to_string())])
            }
        };

        let keybroker = Self::keybroker_from_env();
        if keybroker.is_some() {
            tracing::info!("Instance keys enabled for instance {:?}", instance_name());
        }

        Self {
            host: env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
            port: env::var("PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(3001),
            storage,
            db_path: env::var("DB_PATH").unwrap_or_else(|_| "brickbed".to_string()),
            api_keys,
            keybroker,
            embeddings: env::var("EMBEDDINGS_PROVIDER")
                .ok()
                .map(|name| EmbeddingsConfig::from_env(&name)),
        }
    }

    /// A malformed `INSTANCE_SECRET` is fatal rather than ignored: silently
    /// falling back to static-keys-only would look like working auth while
    /// every minted key is rejected.
    fn keybroker_from_env() -> Option<KeyBroker> {
        let secret = env::var("INSTANCE_SECRET").ok()?;
        Some(
            KeyBroker::from_hex(&instance_name(), &secret)
                .unwrap_or_else(|e| panic!("INSTANCE_SECRET is set but unusable: {}", e)),
        )
    }

    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// `INSTANCE_NAME`, defaulting to `brickbed`. Also read by `keygen`, so keys
/// mint and verify under the same name without extra plumbing.
pub fn instance_name() -> String {
    env::var("INSTANCE_NAME").unwrap_or_else(|_| DEFAULT_INSTANCE_NAME.to_string())
}

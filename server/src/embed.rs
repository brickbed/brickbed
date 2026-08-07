//! Embed-on-write: turn source fields into a vector at write time.
//!
//! A vector validator carrying `{"from": [fields], "model": name}` is
//! server-filled. When a write does not supply the vector itself, the declared
//! source fields are concatenated and sent to the configured provider, and the
//! result is stored on the document like any client-supplied vector.
//!
//! The provider is called before the write batch is built, so a provider
//! failure leaves nothing persisted.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::config::{EmbeddingsConfig, ProviderKind};
use crate::error::AppError;
use crate::schema::CollectionSchema;

/// How long a provider has to answer before the write fails. A write holds the
/// single-writer lock, so this cannot be generous.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Source text is truncated to this many characters. Every provider rejects
/// inputs past some token budget, and a clamped embedding beats a 502 on a
/// long document.
const MAX_SOURCE_CHARS: usize = 8_000;

/// Provider error bodies are echoed back to the caller, so only a prefix is
/// kept: enough to identify the problem, bounded regardless of what came back.
const MAX_ERROR_BODY: usize = 200;

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn name(&self) -> &'static str;

    /// One vector per input text, in the same order.
    async fn embed(&self, texts: &[String], model: &str) -> Result<Vec<Vec<f32>>, AppError>;
}

/// Build the provider a config selects, or `None` when embed-on-write is off.
pub fn from_config(
    config: Option<&EmbeddingsConfig>,
) -> Result<Option<std::sync::Arc<dyn EmbeddingProvider>>, AppError> {
    let Some(config) = config else {
        return Ok(None);
    };
    let provider: std::sync::Arc<dyn EmbeddingProvider> = match config.provider {
        ProviderKind::OpenAi => std::sync::Arc::new(OpenAiProvider::new(
            config.api_key.clone(),
            config.base_url.clone(),
        )?),
        ProviderKind::Cohere => std::sync::Arc::new(CohereProvider::new(
            config.api_key.clone(),
            config.base_url.clone(),
        )?),
        ProviderKind::Mock { dims } => std::sync::Arc::new(MockProvider::new(dims)),
    };
    tracing::info!("embed-on-write enabled via {}", provider.name());
    Ok(Some(provider))
}

// ---- Planning ----

/// A vector field the server fills in for one write.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub field: String,
    pub model: String,
    pub dims: usize,
    pub text: String,
}

/// What a PATCH changed. Absent for insert and replace, which write the whole
/// document and so re-embed whenever the vector itself is missing.
#[derive(Debug, Clone, Copy)]
pub struct PatchContext<'a> {
    pub previous: &'a Map<String, Value>,
    pub updates: &'a Map<String, Value>,
}

/// The embed-on-write settings of a vector validator, if it has any.
struct EmbedSpec {
    dims: usize,
    from: Vec<String>,
    model: String,
}

fn embed_spec(validator: &Value) -> Option<EmbedSpec> {
    let validator = unwrap_optional(validator);
    if validator.get("type").and_then(Value::as_str)? != "vector" {
        return None;
    }
    let dims = validator.get("dims").and_then(Value::as_u64)? as usize;
    let from: Vec<String> = validator
        .get("from")?
        .as_array()?
        .iter()
        .filter_map(|f| f.as_str().map(str::to_string))
        .collect();
    let model = validator.get("model").and_then(Value::as_str)?.to_string();
    if from.is_empty() || model.is_empty() {
        return None;
    }
    Some(EmbedSpec { dims, from, model })
}

fn unwrap_optional(validator: &Value) -> &Value {
    if validator.get("type").and_then(Value::as_str) == Some("optional") {
        return validator.get("inner").unwrap_or(validator);
    }
    validator
}

/// Text a document contributes to an embedding: strings and (nested) arrays of
/// strings, in the order the sources are declared.
fn collect_text(value: &Value, out: &mut String) {
    match value {
        Value::String(s) => {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(s);
        }
        Value::Array(items) => {
            for item in items {
                collect_text(item, out);
            }
        }
        _ => {}
    }
}

fn source_text(data: &Map<String, Value>, from: &[String]) -> String {
    let mut text = String::new();
    for field in from {
        if let Some(value) = data.get(field) {
            collect_text(value, &mut text);
        }
    }
    if text.chars().count() > MAX_SOURCE_CHARS {
        text = text.chars().take(MAX_SOURCE_CHARS).collect();
    }
    text.trim().to_string()
}

/// Whether a document already carries a usable vector for a field.
fn has_vector(data: &Map<String, Value>, field: &str) -> bool {
    !matches!(data.get(field), None | Some(Value::Null))
}

/// What one write needs from the embedding provider.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    /// Fields to embed, with the text to send.
    pub embed: Vec<Pending>,
    /// Fields whose sources no longer hold any text. Any vector stored there
    /// describes text the document no longer has, so it is dropped and the
    /// document leaves the vector index.
    pub clear: Vec<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.embed.is_empty() && self.clear.is_empty()
    }
}

/// Vector work for one write.
///
/// Insert and replace embed whenever the document does not supply the vector.
/// A patch is narrower: it re-embeds only when it changed a source field and
/// did not set the vector itself, so an unrelated patch never calls out to a
/// provider.
pub fn plan(
    schema: &CollectionSchema,
    data: &Map<String, Value>,
    patch: Option<PatchContext<'_>>,
) -> Plan {
    let mut out = Plan::default();
    for (field, validator) in &schema.fields {
        let Some(spec) = embed_spec(validator) else {
            continue;
        };

        let wanted = match patch {
            None => !has_vector(data, field),
            Some(ctx) => {
                !ctx.updates.contains_key(field)
                    && spec
                        .from
                        .iter()
                        .any(|source| data.get(source) != ctx.previous.get(source))
            }
        };
        if !wanted {
            continue;
        }

        let text = source_text(data, &spec.from);
        if text.is_empty() {
            // No text to embed. On a patch that emptied the sources this
            // retires the vector the old text produced; otherwise there was
            // nothing stored and dropping the field is a no-op.
            out.clear.push(field.clone());
            continue;
        }

        out.embed.push(Pending {
            field: field.clone(),
            model: spec.model,
            dims: spec.dims,
            text,
        });
    }
    out
}

/// A provider's vector as a document field, rejecting anything unstorable.
pub fn vector_value(
    provider: &str,
    model: &str,
    pending: &Pending,
    vector: Vec<f32>,
) -> Result<Value, AppError> {
    if vector.len() != pending.dims {
        return Err(AppError::Embedding(format!(
            "{} model {:?} returned {} dimensions but field {:?} declares {}",
            provider,
            model,
            vector.len(),
            pending.field,
            pending.dims
        )));
    }
    if let Some(i) = vector.iter().position(|c| !c.is_finite()) {
        return Err(AppError::Embedding(format!(
            "{} model {:?} returned a non-finite component at index {}",
            provider, model, i
        )));
    }
    Ok(Value::Array(vector.into_iter().map(|c| json!(c)).collect()))
}

// ---- Providers ----

fn http_client() -> Result<reqwest::Client, AppError> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| AppError::Embedding(format!("could not build HTTP client: {}", e)))
}

/// Transport failure. reqwest's own `Display` is just "error sending request",
/// so the source chain is walked: the cause (DNS, TLS, timeout) is the only
/// part anyone can act on. Transport errors carry no credentials.
fn request_error(provider: &str, err: &reqwest::Error) -> AppError {
    let mut detail = err.to_string();
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        detail.push_str(": ");
        detail.push_str(&cause.to_string());
        source = cause.source();
    }
    AppError::Embedding(format!("{} request failed: {}", provider, detail))
}

/// Error text for a non-2xx provider response.
///
/// The body reaches the API client, and the request that produced it carried
/// a bearer token, so the configured key is scrubbed before the body is
/// surfaced: a misconfigured `EMBEDDINGS_BASE_URL` pointing at something that
/// echoes request headers must not turn into credential disclosure.
fn provider_error(
    provider: &str,
    status: reqwest::StatusCode,
    body: &str,
    api_key: &str,
) -> AppError {
    let body: String = body.chars().take(MAX_ERROR_BODY).collect();
    // Replacing an empty needle would splice the marker between every char.
    let body = if api_key.is_empty() {
        body
    } else {
        body.replace(api_key, "[redacted]")
    };
    AppError::Embedding(format!(
        "{} returned {}: {}",
        provider,
        status.as_u16(),
        body.trim()
    ))
}

pub struct OpenAiProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl OpenAiProvider {
    pub fn new(api_key: String, base_url: Option<String>) -> Result<Self, AppError> {
        Ok(Self {
            client: http_client()?,
            api_key,
            base_url: base_url.unwrap_or_else(|| "https://api.openai.com".to_string()),
        })
    }
}

#[derive(Deserialize)]
struct OpenAiResponse {
    data: Vec<OpenAiEmbedding>,
}

#[derive(Deserialize)]
struct OpenAiEmbedding {
    index: usize,
    embedding: Vec<f32>,
}

#[async_trait]
impl EmbeddingProvider for OpenAiProvider {
    fn name(&self) -> &'static str {
        "openai"
    }

    async fn embed(&self, texts: &[String], model: &str) -> Result<Vec<Vec<f32>>, AppError> {
        let response = self
            .client
            .post(format!("{}/v1/embeddings", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&json!({ "model": model, "input": texts }))
            .send()
            .await
            .map_err(|e| request_error("openai", &e))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(provider_error("openai", status, &body, &self.api_key));
        }

        let parsed: OpenAiResponse = response
            .json()
            .await
            .map_err(|e| AppError::Embedding(format!("openai response was unreadable: {}", e)))?;

        // The API documents `index`; sorting on it keeps the mapping back to
        // the input texts correct regardless of response order.
        let mut data = parsed.data;
        data.sort_by_key(|e| e.index);
        Ok(data.into_iter().map(|e| e.embedding).collect())
    }
}

pub struct CohereProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl CohereProvider {
    pub fn new(api_key: String, base_url: Option<String>) -> Result<Self, AppError> {
        Ok(Self {
            client: http_client()?,
            api_key,
            base_url: base_url.unwrap_or_else(|| "https://api.cohere.com".to_string()),
        })
    }
}

#[derive(Deserialize)]
struct CohereResponse {
    embeddings: CohereEmbeddings,
}

#[derive(Deserialize)]
struct CohereEmbeddings {
    float: Vec<Vec<f32>>,
}

#[async_trait]
impl EmbeddingProvider for CohereProvider {
    fn name(&self) -> &'static str {
        "cohere"
    }

    async fn embed(&self, texts: &[String], model: &str) -> Result<Vec<Vec<f32>>, AppError> {
        let response = self
            .client
            .post(format!("{}/v2/embed", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&json!({
                "model": model,
                "texts": texts,
                // Documents are indexed for retrieval; queries are embedded
                // client-side today, so this is always the document side.
                "input_type": "search_document",
                "embedding_types": ["float"],
            }))
            .send()
            .await
            .map_err(|e| request_error("cohere", &e))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(provider_error("cohere", status, &body, &self.api_key));
        }

        let parsed: CohereResponse = response
            .json()
            .await
            .map_err(|e| AppError::Embedding(format!("cohere response was unreadable: {}", e)))?;
        Ok(parsed.embeddings.float)
    }
}

/// Deterministic provider for tests and offline development: identical text
/// always yields an identical vector, different text points elsewhere, and
/// nothing leaves the process. Not for production use.
pub struct MockProvider {
    dims: usize,
    failure: Option<String>,
}

impl MockProvider {
    pub fn new(dims: usize) -> Self {
        Self {
            dims,
            failure: None,
        }
    }

    /// A provider that always fails, for exercising the write-path error path.
    pub fn failing(message: &str) -> Self {
        Self {
            dims: 0,
            failure: Some(message.to_string()),
        }
    }
}

/// FNV-1a over the text, then an LCG per component: stable across runs and
/// platforms, and spread enough that different text points in different
/// directions.
fn deterministic_vector(text: &str, dims: usize) -> Vec<f32> {
    let mut state = 0xcbf2_9ce4_8422_2325u64;
    for byte in text.as_bytes() {
        state ^= u64::from(*byte);
        state = state.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (0..dims)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let unit = (state >> 33) as f32 / (1u64 << 31) as f32;
            2.0 * unit - 1.0
        })
        .collect()
}

#[async_trait]
impl EmbeddingProvider for MockProvider {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn embed(&self, texts: &[String], _model: &str) -> Result<Vec<Vec<f32>>, AppError> {
        if let Some(message) = &self.failure {
            return Err(AppError::Embedding(message.clone()));
        }
        Ok(texts
            .iter()
            .map(|text| deterministic_vector(text, self.dims))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::CollectionSchema;
    use std::collections::BTreeMap;

    fn schema_with(validator: Value) -> CollectionSchema {
        CollectionSchema {
            fields: BTreeMap::from([
                ("title".to_string(), json!({"type": "string"})),
                ("body".to_string(), json!({"type": "string"})),
                ("slug".to_string(), json!({"type": "string"})),
                ("embedding".to_string(), validator),
            ]),
            indexes: Vec::new(),
            search_indexes: Vec::new(),
            vector_indexes: Vec::new(),
            rules: None,
        }
    }

    fn embedded_field() -> Value {
        json!({"type": "vector", "dims": 4, "from": ["title", "body"], "model": "test-model"})
    }

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn full_writes_embed_only_when_the_vector_is_missing() {
        let schema = schema_with(embedded_field());

        let without = object(json!({"title": "Rust", "body": "Storage"}));
        let planned = plan(&schema, &without, None);
        assert_eq!(planned.embed.len(), 1);
        assert_eq!(planned.embed[0].field, "embedding");
        assert_eq!(planned.embed[0].model, "test-model");
        assert_eq!(planned.embed[0].dims, 4);
        assert_eq!(planned.embed[0].text, "Rust Storage");

        // A client-supplied vector is left alone; an explicit null means the
        // document has none, so the server fills it.
        let with = object(json!({"title": "Rust", "body": "Storage", "embedding": [1, 2, 3, 4]}));
        assert!(plan(&schema, &with, None).is_empty());

        let null = object(json!({"title": "Rust", "body": "Storage", "embedding": null}));
        assert_eq!(plan(&schema, &null, None).embed.len(), 1);

        // No source text at all: nothing to send, nothing stored to retire.
        let empty = object(json!({"slug": "x"}));
        let planned = plan(&schema, &empty, None);
        assert!(planned.embed.is_empty());
        assert_eq!(planned.clear, vec!["embedding".to_string()]);
    }

    #[test]
    fn a_vector_without_embed_settings_is_never_server_filled() {
        for validator in [
            json!({"type": "vector", "dims": 4}),
            json!({"type": "vector", "dims": 4, "from": [], "model": "m"}),
            json!({"type": "vector", "dims": 4, "from": ["title"], "model": ""}),
            json!({"type": "string"}),
        ] {
            let schema = schema_with(validator.clone());
            let data = object(json!({"title": "Rust"}));
            assert!(
                plan(&schema, &data, None).is_empty(),
                "validator: {}",
                validator
            );
        }
    }

    #[test]
    fn optional_wrappers_carry_the_embed_settings() {
        let schema = schema_with(json!({"type": "optional", "inner": embedded_field()}));
        let data = object(json!({"title": "Rust", "body": "Storage"}));
        assert_eq!(plan(&schema, &data, None).embed.len(), 1);
    }

    #[test]
    fn patches_re_embed_only_on_a_changed_source() {
        let schema = schema_with(embedded_field());
        let previous =
            object(json!({"title": "Rust", "body": "Storage", "embedding": [1, 2, 3, 4]}));

        // Source changed: re-embed even though a vector is already stored.
        let updates = object(json!({"title": "Rust rewritten"}));
        let mut merged = previous.clone();
        merged.insert("title".to_string(), json!("Rust rewritten"));
        let ctx = PatchContext {
            previous: &previous,
            updates: &updates,
        };
        let planned = plan(&schema, &merged, Some(ctx));
        assert_eq!(planned.embed.len(), 1);
        assert_eq!(planned.embed[0].text, "Rust rewritten Storage");

        // Unrelated field: no provider call.
        let updates = object(json!({"slug": "new-slug"}));
        let mut merged = previous.clone();
        merged.insert("slug".to_string(), json!("new-slug"));
        let ctx = PatchContext {
            previous: &previous,
            updates: &updates,
        };
        assert!(plan(&schema, &merged, Some(ctx)).is_empty());

        // Source rewritten to the same value is not a change.
        let updates = object(json!({"title": "Rust"}));
        let ctx = PatchContext {
            previous: &previous,
            updates: &updates,
        };
        assert!(plan(&schema, &previous, Some(ctx)).is_empty());

        // An explicit vector in the patch wins over a changed source.
        let updates = object(json!({"title": "Rewritten", "embedding": [9, 9, 9, 9]}));
        let mut merged = previous.clone();
        merged.insert("title".to_string(), json!("Rewritten"));
        merged.insert("embedding".to_string(), json!([9, 9, 9, 9]));
        let ctx = PatchContext {
            previous: &previous,
            updates: &updates,
        };
        assert!(plan(&schema, &merged, Some(ctx)).is_empty());
    }

    #[test]
    fn emptying_the_sources_retires_a_stored_vector() {
        let schema = schema_with(embedded_field());
        let previous =
            object(json!({"title": "Rust", "body": "Storage", "embedding": [1, 2, 3, 4]}));

        // Clearing every source leaves nothing to embed. The stored vector
        // describes text the document no longer has, so it must not survive.
        for cleared in [json!(""), json!(null)] {
            let updates = object(json!({"title": cleared, "body": cleared}));
            let mut merged = previous.clone();
            merged.insert("title".to_string(), cleared.clone());
            merged.insert("body".to_string(), cleared.clone());
            let ctx = PatchContext {
                previous: &previous,
                updates: &updates,
            };

            let planned = plan(&schema, &merged, Some(ctx));
            assert!(planned.embed.is_empty(), "cleared: {}", cleared);
            assert_eq!(
                planned.clear,
                vec!["embedding".to_string()],
                "cleared: {}",
                cleared
            );
        }

        // One remaining source is still enough to re-embed from.
        let updates = object(json!({"title": ""}));
        let mut merged = previous.clone();
        merged.insert("title".to_string(), json!(""));
        let ctx = PatchContext {
            previous: &previous,
            updates: &updates,
        };
        let planned = plan(&schema, &merged, Some(ctx));
        assert!(planned.clear.is_empty());
        assert_eq!(planned.embed[0].text, "Storage");
    }

    #[test]
    fn source_text_concatenates_strings_and_arrays_and_is_bounded() {
        let data = object(json!({
            "title": "Rust",
            "body": "Storage engine",
            "tags": ["fast", "cheap"],
            "count": 7
        }));
        assert_eq!(
            source_text(&data, &["title".to_string(), "body".to_string()]),
            "Rust Storage engine"
        );
        assert_eq!(
            source_text(&data, &["tags".to_string()]),
            "fast cheap",
            "arrays of strings contribute"
        );
        assert_eq!(
            source_text(&data, &["count".to_string()]),
            "",
            "non-text fields contribute nothing"
        );
        assert_eq!(source_text(&data, &["absent".to_string()]), "");

        let long = object(json!({ "title": "x".repeat(MAX_SOURCE_CHARS + 500) }));
        assert_eq!(
            source_text(&long, &["title".to_string()]).chars().count(),
            MAX_SOURCE_CHARS
        );
    }

    #[tokio::test]
    async fn mock_provider_is_deterministic_and_can_fail() {
        let provider = MockProvider::new(4);
        let texts = vec!["hello".to_string(), "world".to_string()];

        let first = provider.embed(&texts, "any").await.unwrap();
        let second = provider.embed(&texts, "any").await.unwrap();
        assert_eq!(first, second, "same text must embed identically");
        assert_ne!(first[0], first[1], "different text must embed differently");
        assert_eq!(first[0].len(), 4);
        assert!(first[0].iter().all(|c| c.is_finite() && c.abs() <= 1.0));

        let broken = MockProvider::failing("provider is down");
        let err = broken.embed(&texts, "any").await.unwrap_err();
        assert!(matches!(err, AppError::Embedding(msg) if msg == "provider is down"));
    }

    /// Envelopes captured from the live APIs on 2026-08-07 (vectors trimmed to
    /// three components). The mock cannot catch a renamed field, so the real
    /// payloads are parsed here by the same structs the client uses.
    #[test]
    fn provider_response_envelopes_parse() {
        let openai = r#"{
            "object": "list",
            "data": [
                {"object": "embedding", "index": 1, "embedding": [0.25, 0.5, 0.75]},
                {"object": "embedding", "index": 0, "embedding": [-0.031585693, 0.03594971, 0.0577392]}
            ],
            "model": "text-embedding-3-small",
            "usage": {"prompt_tokens": 18, "total_tokens": 18}
        }"#;
        let parsed: OpenAiResponse = serde_json::from_str(openai).expect("openai envelope");
        let mut data = parsed.data;
        data.sort_by_key(|e| e.index);
        // Deliberately out of order above: the response is keyed by `index`,
        // so sorting is what keeps vectors aligned with their input texts.
        assert_eq!(data[0].index, 0);
        assert_eq!(data[0].embedding, vec![-0.031585693, 0.03594971, 0.0577392]);
        assert_eq!(data[1].embedding, vec![0.25, 0.5, 0.75]);

        let cohere = r#"{
            "id": "abc-123",
            "texts": ["one", "two"],
            "embeddings": {
                "float": [
                    [-0.018647103, -0.0047445293, -0.0050479583],
                    [0.25, 0.5, 0.75]
                ]
            },
            "meta": {"api_version": {"version": "2"}},
            "response_type": "embeddings_by_type"
        }"#;
        let parsed: CohereResponse = serde_json::from_str(cohere).expect("cohere envelope");
        assert_eq!(parsed.embeddings.float.len(), 2);
        assert_eq!(
            parsed.embeddings.float[0],
            vec![-0.018647103, -0.0047445293, -0.0050479583]
        );
        assert_eq!(parsed.embeddings.float[1], vec![0.25, 0.5, 0.75]);
    }

    #[test]
    fn provider_errors_never_echo_the_api_key() {
        let status = reqwest::StatusCode::UNAUTHORIZED;

        // A proxy or provider that reflects the request must not turn into
        // credential disclosure through the API response.
        let body = r#"{"error":"bad token","seen":"Bearer sk-secret-value"}"#;
        let err = provider_error("openai", status, body, "sk-secret-value");
        let AppError::Embedding(message) = err else {
            panic!("expected an embedding error");
        };
        assert!(!message.contains("sk-secret-value"), "message: {}", message);
        assert!(message.contains("[redacted]"), "message: {}", message);
        assert!(message.contains("401"), "message: {}", message);

        // An empty key must not splice the marker between every character.
        let err = provider_error("mock", status, "plain body", "");
        let AppError::Embedding(message) = err else {
            panic!("expected an embedding error");
        };
        assert!(message.ends_with("plain body"), "message: {}", message);

        // Bodies are bounded regardless of what came back.
        let long = "x".repeat(MAX_ERROR_BODY * 2);
        let err = provider_error("openai", status, &long, "key");
        let AppError::Embedding(message) = err else {
            panic!("expected an embedding error");
        };
        assert!(message.len() < MAX_ERROR_BODY * 2, "message: {}", message);
    }

    #[test]
    fn vector_value_rejects_the_wrong_shape() {
        let pending = Pending {
            field: "embedding".to_string(),
            model: "test-model".to_string(),
            dims: 3,
            text: "x".to_string(),
        };

        let ok = vector_value("mock", "test-model", &pending, vec![0.5, -0.5, 0.0]).unwrap();
        assert_eq!(ok, json!([0.5, -0.5, 0.0]));

        let wrong_width = vector_value("mock", "test-model", &pending, vec![0.5, -0.5]);
        assert!(matches!(wrong_width, Err(AppError::Embedding(msg)) if msg.contains("dimensions")));

        let not_finite = vector_value("mock", "test-model", &pending, vec![0.5, f32::NAN, 0.0]);
        assert!(matches!(not_finite, Err(AppError::Embedding(msg)) if msg.contains("non-finite")));
    }
}

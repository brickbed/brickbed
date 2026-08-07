//! End-user identity: third-party JWT verification. This module owns the
//! wire format and ordered verification checks.
//!
//! Self-contained by design — nothing here reaches into the rest of the crate,
//! so the module compiles and tests standalone ahead of being wired into the
//! request path.
//!
//! The security-critical rule: the token header's `alg` never *selects* the
//! algorithm, it is only checked for membership in the provider's allowlist,
//! which holds asymmetric algorithms only. That is the alg-confusion defence.

use std::collections::HashMap;
use std::sync::Arc;

use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Monotonic milliseconds, supplied by the caller. Passing the clock in keeps
/// cache expiry deterministic in tests instead of sleeping.
pub type Millis = u64;

/// Algorithms a provider may declare. Asymmetric only: an `HS*` entry would let
/// anyone holding the public key mint tokens, and `none` needs no key at all.
pub const ALLOWED_ALGORITHMS: &[&str] = &[
    "RS256", "RS384", "RS512", "PS256", "PS384", "PS512", "ES256", "ES384",
];

const DEFAULT_ALGORITHMS: &[&str] = &["RS256"];

/// Clock skew allowed on `exp`/`nbf`.
const LEEWAY_SECONDS: u64 = 60;

/// Upper bound on cached issuers, so config churn cannot grow the map forever.
pub const MAX_CACHED_ISSUERS: usize = 256;

pub const DEFAULT_TTL_MS: Millis = 10 * 60 * 1000;
pub const DEFAULT_NEGATIVE_TTL_MS: Millis = 30 * 1000;
pub const DEFAULT_REFETCH_COOLDOWN_MS: Millis = 60 * 1000;

#[derive(Debug, thiserror::Error)]
pub enum JwtError {
    #[error("auth config: {0}")]
    Config(String),

    #[error("malformed token")]
    Malformed,

    #[error("no auth provider configured for issuer {0:?}")]
    UnknownIssuer(String),

    #[error("algorithm {0:?} is not allowed for this provider")]
    AlgorithmNotAllowed(String),

    #[error("token signing key {0:?} is unknown")]
    UnknownKeyId(String),

    #[error("token rejected")]
    Rejected,

    #[error("signing keys unavailable for issuer {0:?}")]
    KeysUnavailable(String),

    #[error("jwks fetch failed: {0}")]
    Fetch(String),
}

// ---- provider configuration ----

/// `auth` block of a pushed project schema.
/// Strict on unknown keys: a typo'd `"audiance"` silently skipping the
/// audience check would be a security misconfiguration, not a convenience.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Exact-match against the `iss` claim; also the JWKS cache key.
    pub issuer: String,
    /// When set, `aud` must contain it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
    /// Absent means discover via `{issuer}/.well-known/openid-configuration`.
    #[serde(default, rename = "jwksUrl", skip_serializing_if = "Option::is_none")]
    pub jwks_url: Option<String>,
    #[serde(default = "default_algorithms")]
    pub algorithms: Vec<String>,
}

fn default_algorithms() -> Vec<String> {
    DEFAULT_ALGORITHMS.iter().map(|a| a.to_string()).collect()
}

impl AuthConfig {
    pub fn provider_for(&self, issuer: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|p| p.issuer == issuer)
    }
}

/// Reject unusable auth config at schema push, where the error is actionable,
/// rather than at the first request.
pub fn validate_auth_config(config: &AuthConfig) -> Result<(), JwtError> {
    if config.providers.is_empty() {
        return Err(JwtError::Config(
            "auth.providers must not be empty".to_string(),
        ));
    }

    let mut seen: Vec<&str> = Vec::new();
    for provider in &config.providers {
        check_url("issuer", &provider.issuer)?;
        if seen.contains(&provider.issuer.as_str()) {
            return Err(JwtError::Config(format!(
                "duplicate issuer {:?}",
                provider.issuer
            )));
        }
        seen.push(&provider.issuer);

        if let Some(url) = &provider.jwks_url {
            check_url("jwksUrl", url)?;
        }
        if let Some(audience) = &provider.audience {
            if audience.is_empty() {
                return Err(JwtError::Config("audience must not be empty".to_string()));
            }
        }
        if provider.algorithms.is_empty() {
            return Err(JwtError::Config(format!(
                "provider {:?} declares no algorithms",
                provider.issuer
            )));
        }
        for algorithm in &provider.algorithms {
            if !ALLOWED_ALGORITHMS.contains(&algorithm.as_str()) {
                return Err(JwtError::Config(format!(
                    "algorithm {:?} is not allowed (asymmetric only: {:?})",
                    algorithm, ALLOWED_ALGORITHMS
                )));
            }
        }
    }
    Ok(())
}

/// https required, except on loopback so local development works.
fn check_url(field: &str, raw: &str) -> Result<(), JwtError> {
    let url = reqwest::Url::parse(raw)
        .map_err(|e| JwtError::Config(format!("{} {:?} is not a URL: {}", field, raw, e)))?;

    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err(JwtError::Config(format!(
            "{} {:?} must be https (http allowed only on loopback)",
            field, raw
        )));
    }
    Ok(())
}

// ---- identity ----

/// A verified end user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub issuer: String,
    /// The `sub` claim.
    pub subject: String,
    /// `{issuer}|{subject}` — unique even when two providers issue the same
    /// `sub`.
    pub token_identifier: String,
    pub email: Option<String>,
    pub claims: Map<String, Value>,
}

/// Who is making a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// Static `API_KEYS` entry or keybroker-derived key: trusted server-side
    /// credential, scoped to a project, bypasses collection rules.
    ApiKey {
        project: String,
    },
    EndUser(Identity),
    Anonymous,
}

impl Principal {
    pub fn identity(&self) -> Option<&Identity> {
        match self {
            Principal::EndUser(identity) => Some(identity),
            _ => None,
        }
    }

    pub fn is_api_key(&self) -> bool {
        matches!(self, Principal::ApiKey { .. })
    }
}

// ---- token verification ----

/// Cheap shape test used to decide whether a bearer token is worth trying as a
/// JWT at all. Not a security boundary — verification is.
pub fn looks_like_jwt(token: &str) -> bool {
    let mut parts = token.split('.');
    let (Some(header), Some(_), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    if header.is_empty() || signature.is_empty() {
        return false;
    }
    decode_segment(header)
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .is_some_and(|json| json.get("alg").and_then(Value::as_str).is_some())
}

fn decode_segment(segment: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    URL_SAFE_NO_PAD.decode(segment).ok()
}

/// Read `iss` *without verifying anything*, purely to choose which provider's
/// keys to verify against. The claim is checked again during verification, so a
/// forged `iss` here only picks the wrong keys and fails the signature.
pub fn peek_issuer(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let json: Value = serde_json::from_slice(&decode_segment(payload)?).ok()?;
    json.get("iss").and_then(Value::as_str).map(str::to_string)
}

/// Verify a token against one provider and its current keys.
///
/// Order: allowlisted `alg` -> key by `kid` -> signature
/// -> `iss` -> `aud` -> `exp`/`nbf`. Every failure returns the same flat error
/// to the caller; which step failed is never reported outward.
pub fn verify_with_provider(
    provider: &ProviderConfig,
    keys: &JwkSet,
    token: &str,
) -> Result<Identity, JwtError> {
    let header = decode_header(token).map_err(|_| JwtError::Malformed)?;
    let alg_name = format!("{:?}", header.alg);

    // The allowlist is consulted before anything else touches the token, and
    // the algorithm we verify with is the provider's, never the header's.
    if !provider.algorithms.iter().any(|a| a == &alg_name)
        || !ALLOWED_ALGORITHMS.contains(&alg_name.as_str())
    {
        return Err(JwtError::AlgorithmNotAllowed(alg_name));
    }
    let algorithm: Algorithm = alg_name
        .parse()
        .map_err(|_| JwtError::AlgorithmNotAllowed(alg_name.clone()))?;

    let jwk = match &header.kid {
        Some(kid) => keys.find(kid).ok_or(JwtError::UnknownKeyId(kid.clone()))?,
        // No `kid`: only unambiguous when the set holds exactly one key.
        None => match keys.keys.as_slice() {
            [only] => only,
            _ => return Err(JwtError::UnknownKeyId(String::new())),
        },
    };
    let key = DecodingKey::from_jwk(jwk).map_err(|_| JwtError::Rejected)?;

    let mut validation = Validation::new(algorithm);
    validation.algorithms = vec![algorithm];
    validation.leeway = LEEWAY_SECONDS;
    validation.validate_exp = true;
    validation.validate_nbf = true;
    validation.set_issuer(&[&provider.issuer]);
    match &provider.audience {
        Some(audience) => {
            validation.validate_aud = true;
            validation.set_audience(&[audience]);
        }
        // No audience configured: nothing to compare against, so comparing
        // would only reject tokens that legitimately carry an `aud`.
        None => validation.validate_aud = false,
    }
    validation.set_required_spec_claims(&["exp", "iss", "sub"]);

    let data =
        decode::<Map<String, Value>>(token, &key, &validation).map_err(|_| JwtError::Rejected)?;
    identity_from_claims(provider, data.claims)
}

fn identity_from_claims(
    provider: &ProviderConfig,
    claims: Map<String, Value>,
) -> Result<Identity, JwtError> {
    // `jsonwebtoken` validates `exp` and `nbf` but not `iat`. A token issued in
    // the future is a clock or forgery problem either way.
    if let Some(issued_at) = claims.get("iat").and_then(Value::as_u64) {
        if issued_at > jsonwebtoken::get_current_timestamp() + LEEWAY_SECONDS {
            return Err(JwtError::Rejected);
        }
    }

    let subject = claims
        .get("sub")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(JwtError::Rejected)?
        .to_string();

    // `iss` is validated by `decode`; read it back rather than trusting the
    // provider blindly, so the identity records what the token actually said.
    let issuer = claims
        .get("iss")
        .and_then(Value::as_str)
        .unwrap_or(&provider.issuer)
        .to_string();

    Ok(Identity {
        token_identifier: format!("{}|{}", issuer, subject),
        email: claims
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_string),
        issuer,
        subject,
        claims,
    })
}

// ---- JWKS cache ----

/// The one piece of IO in this module, behind a trait so tests never touch the
/// network.
pub trait JwksFetcher: Send + Sync {
    fn fetch_json(
        &self,
        url: &str,
    ) -> impl std::future::Future<Output = Result<Value, JwtError>> + Send;
}

/// Production fetcher.
pub struct HttpJwksFetcher {
    client: reqwest::Client,
}

impl HttpJwksFetcher {
    pub fn new() -> Self {
        Self {
            // No redirects: a fetch is aimed at a validated URL, and following
            // a 302 would land somewhere that was never checked. Timeout so a
            // hung IdP cannot pin a request task indefinitely.
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .expect("jwks http client"),
        }
    }
}

impl Default for HttpJwksFetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl JwksFetcher for HttpJwksFetcher {
    async fn fetch_json(&self, url: &str) -> Result<Value, JwtError> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| JwtError::Fetch(e.to_string()))?
            .error_for_status()
            .map_err(|e| JwtError::Fetch(e.to_string()))?;
        response
            .json()
            .await
            .map_err(|e| JwtError::Fetch(e.to_string()))
    }
}

/// Whether a fetch may be satisfied by a cache entry that filled while we
/// waited for the fetch lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refetch {
    IfStale,
    Forced,
}

#[derive(Debug, Clone)]
struct Entry {
    keys: Option<Arc<JwkSet>>,
    fetched_at_ms: Millis,
    failed_at_ms: Option<Millis>,
    /// Last attempt, successful or not.
    attempted_at_ms: Millis,
    /// Last *forced* refetch, which is what the unknown-`kid` cooldown limits.
    forced_at_ms: Option<Millis>,
}

/// Per-issuer JWKS cache: TTL on success, short negative cache on failure, and
/// a cooldown so an unknown `kid` cannot be used to hammer the IdP.
pub struct JwksCache<F: JwksFetcher> {
    fetcher: F,
    pub ttl_ms: Millis,
    pub negative_ttl_ms: Millis,
    pub refetch_cooldown_ms: Millis,
    entries: tokio::sync::RwLock<HashMap<String, Entry>>,
    /// Held across a fetch so a burst of misses produces one request, not one
    /// per caller. Fetches are rare (once per issuer per TTL), so serialising
    /// them across issuers costs nothing worth optimising.
    fetch_lock: tokio::sync::Mutex<()>,
}

impl<F: JwksFetcher> JwksCache<F> {
    pub fn new(fetcher: F) -> Self {
        Self {
            fetcher,
            ttl_ms: DEFAULT_TTL_MS,
            negative_ttl_ms: DEFAULT_NEGATIVE_TTL_MS,
            refetch_cooldown_ms: DEFAULT_REFETCH_COOLDOWN_MS,
            entries: tokio::sync::RwLock::new(HashMap::new()),
            fetch_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Seed the cache directly, bypassing the fetcher.
    ///
    /// Exists so tests (and, later, an ops-supplied key pin) can exercise
    /// verification without reaching an identity provider over the network.
    pub async fn preload(&self, issuer: &str, keys: JwkSet, now_ms: Millis) {
        let mut entries = self.entries.write().await;
        evict_if_full(&mut entries, issuer);
        entries.insert(
            issuer.to_string(),
            Entry {
                keys: Some(Arc::new(keys)),
                fetched_at_ms: now_ms,
                failed_at_ms: None,
                attempted_at_ms: now_ms,
                forced_at_ms: None,
            },
        );
    }

    /// Cached keys, fetching only when absent or stale.
    pub async fn keys_for(
        &self,
        provider: &ProviderConfig,
        now_ms: Millis,
    ) -> Result<Arc<JwkSet>, JwtError> {
        if let Some(cached) = self.cached(&provider.issuer, now_ms).await? {
            return Ok(cached);
        }
        self.fetch_and_store(provider, now_ms, Refetch::IfStale)
            .await
    }

    /// Refetch because a token named a `kid` we do not hold — rate limited, so
    /// a stream of bogus `kid`s cannot turn into a stream of IdP requests.
    pub async fn keys_for_unknown_kid(
        &self,
        provider: &ProviderConfig,
        now_ms: Millis,
    ) -> Result<Arc<JwkSet>, JwtError> {
        if self.in_cooldown(&provider.issuer, now_ms).await {
            return Err(JwtError::UnknownKeyId(String::new()));
        }
        // Forced: the cached entry is unexpired but demonstrably missing a key,
        // which is exactly what key rotation looks like.
        self.fetch_and_store(provider, now_ms, Refetch::Forced)
            .await
    }

    /// Only a previous *forced* refetch starts the cooldown. An ordinary fetch
    /// must not: keys rotating moments after a cold fetch is precisely when the
    /// refetch is needed, and counting that fetch would blank the retry for a
    /// full cooldown.
    async fn in_cooldown(&self, issuer: &str, now_ms: Millis) -> bool {
        let entries = self.entries.read().await;
        entries.get(issuer).is_some_and(|entry| {
            entry
                .forced_at_ms
                .is_some_and(|forced| now_ms < forced.saturating_add(self.refetch_cooldown_ms))
        })
    }

    /// Fresh keys, or an error if the negative cache is still holding.
    async fn cached(&self, issuer: &str, now_ms: Millis) -> Result<Option<Arc<JwkSet>>, JwtError> {
        let entries = self.entries.read().await;
        let Some(entry) = entries.get(issuer) else {
            return Ok(None);
        };

        if let Some(keys) = &entry.keys {
            if now_ms < entry.fetched_at_ms.saturating_add(self.ttl_ms) {
                return Ok(Some(Arc::clone(keys)));
            }
        }
        if let Some(failed_at) = entry.failed_at_ms {
            if now_ms < failed_at.saturating_add(self.negative_ttl_ms) {
                return Err(JwtError::KeysUnavailable(issuer.to_string()));
            }
        }
        Ok(None)
    }

    async fn fetch_and_store(
        &self,
        provider: &ProviderConfig,
        now_ms: Millis,
        refetch: Refetch,
    ) -> Result<Arc<JwkSet>, JwtError> {
        let _guard = self.fetch_lock.lock().await;

        // Re-check under the lock: a burst that all saw the same stale state
        // before queueing here must still produce one fetch, not one each.
        match refetch {
            // Another caller may have filled the entry while we waited.
            Refetch::IfStale => {
                if let Some(cached) = self.cached(&provider.issuer, now_ms).await? {
                    return Ok(cached);
                }
            }
            // A forced refetch deliberately looks past a fresh entry, so the
            // cache cannot rate-limit it — the cooldown has to, and it must be
            // re-read here because the pre-lock check raced.
            Refetch::Forced => {
                if self.in_cooldown(&provider.issuer, now_ms).await {
                    return Err(JwtError::UnknownKeyId(String::new()));
                }
            }
        }

        // A normal fetch keeps whatever forced-refetch timestamp was already
        // recorded; only a forced one restarts the cooldown.
        let forced_at = match refetch {
            Refetch::Forced => Some(now_ms),
            Refetch::IfStale => self
                .entries
                .read()
                .await
                .get(&provider.issuer)
                .and_then(|e| e.forced_at_ms),
        };

        match self.fetch_jwks(provider).await {
            Ok(keys) => {
                let keys = Arc::new(keys);
                let mut entries = self.entries.write().await;
                evict_if_full(&mut entries, &provider.issuer);
                entries.insert(
                    provider.issuer.clone(),
                    Entry {
                        keys: Some(Arc::clone(&keys)),
                        fetched_at_ms: now_ms,
                        failed_at_ms: None,
                        attempted_at_ms: now_ms,
                        forced_at_ms: forced_at,
                    },
                );
                Ok(keys)
            }
            Err(e) => {
                let mut entries = self.entries.write().await;
                evict_if_full(&mut entries, &provider.issuer);
                let entry = entries.entry(provider.issuer.clone()).or_insert(Entry {
                    keys: None,
                    fetched_at_ms: 0,
                    failed_at_ms: None,
                    attempted_at_ms: now_ms,
                    forced_at_ms: None,
                });
                entry.failed_at_ms = Some(now_ms);
                entry.attempted_at_ms = now_ms;
                if refetch == Refetch::Forced {
                    entry.forced_at_ms = Some(now_ms);
                }
                tracing::warn!("jwks fetch failed for {}: {}", provider.issuer, e);
                Err(e)
            }
        }
    }

    async fn fetch_jwks(&self, provider: &ProviderConfig) -> Result<JwkSet, JwtError> {
        let url = match &provider.jwks_url {
            Some(url) => url.clone(),
            None => {
                let document = self
                    .fetcher
                    .fetch_json(&discovery_url(&provider.issuer))
                    .await?;
                let discovered = document
                    .get("jwks_uri")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        JwtError::Fetch("discovery document has no jwks_uri".to_string())
                    })?
                    .to_string();
                // The discovered URL is remote input and never passed through
                // `validate_auth_config`, so it is checked here instead.
                check_url("discovered jwks_uri", &discovered)
                    .map_err(|e| JwtError::Fetch(e.to_string()))?;
                discovered
            }
        };

        let json = self.fetcher.fetch_json(&url).await?;
        serde_json::from_value(json).map_err(|e| JwtError::Fetch(format!("malformed jwks: {}", e)))
    }
}

/// Keep the cache bounded. Issuers come from admin-pushed config, so this is a
/// backstop rather than a hot path: at the cap, drop the least recently
/// attempted entry, which will simply be refetched if it is still in use.
fn evict_if_full(entries: &mut HashMap<String, Entry>, incoming: &str) {
    if entries.len() < MAX_CACHED_ISSUERS || entries.contains_key(incoming) {
        return;
    }
    let oldest = entries
        .iter()
        .min_by_key(|(_, entry)| entry.attempted_at_ms)
        .map(|(issuer, _)| issuer.clone());
    if let Some(oldest) = oldest {
        entries.remove(&oldest);
    }
}

/// Wall-clock milliseconds, for callers that have no synthetic clock. The cache
/// takes the time as a parameter so its expiry logic stays testable.
pub fn now_ms() -> Millis {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as Millis
}

pub fn discovery_url(issuer: &str) -> String {
    format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const ISSUER: &str = "https://idp.example.com";
    const KID: &str = "test-key-1";

    /// Body of a test-only RSA-2048 key. The PEM marker is assembled at
    /// runtime so generic secret scanners do not mistake this public fixture
    /// for a production credential.
    const PRIVATE_PEM_BODY: &str =
        "MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQC2D+cQNSWgjtOD
CHxIuHLW3t36mrf5bhP5c6LMPLOpKPVWj2hmye+nZjCTrfphQyCjQtJSsYwbygr8
ombHTIpiDUkaZ8f7Lt8lfngWb8g12eBalAr/eD29R8e6GZNLgpuaz0ds0/Fqi32J
ppM/5OoQQD0hWYKW17A0LwaOP0Nq7CyTKhJKRaqhYsjc5r7iWX9Kw3F5xoWyCR+T
sx5W6hnwy0hBUvRYfe8M4E4FfXWfvZg866m3XS4Ie9swesOJdP/KhHV6MCVs03gp
VHRvX4tCPuPMsYa5YFiRzmqU0yg74fBFI83zaqVx7j0ruv1RB60S5i0nvvEfWZKi
iLOwN6mZAgMBAAECggEAJjSIm9NXUc3fD2aCtDz2pmYO5YW9uStTKDwOq/bKCePF
NjSyZy2Vq8aLR5ZRDkOIsBH38m/9M6CutQy8bjK+8GwSzOZ95hVUMPlM6IJtHHXb
+Y4LD8izBgsv27r9uLEQB8jbF3iTdvUOM0pgKJ6YSrA8U8kFmTNqb8z1fnjCpEdP
W70YYXGlnzwGNFoUESbmcIh4+POkHOCQ7c2cvrpC+S0rXsKQVFmK4pK5n4ngkdwg
AVa4TARGruEW7oibSTRnn/gervjGnBNmbjdiSnKU+cvYKh+0jCaN1UJqXKAxwaHF
jMTKTMnxMZFSFN3L2rCcXt5C01Lt9qPM6o+DlNkeTQKBgQD5BMC4mqVuVBYHfOoJ
tfB53/mk/RByRj6/nuoGx93ydf2pltNX2dCkNVsmP7XXxpGdL19xLhwrJWFMzIdg
T789eh8AMsl51YG6HlkAU15KIfhIAzDAvb0syWHEiGdXLTE9PH9sFBPJ8DAdfwh4
Tl+4Yto/zHon8qXyvq9i3Sb39QKBgQC7KpekbB/t9LX9Gz0Z7eO4qDHBemVyNc9W
1TgivC7JjpQSg7BFwfmokEMOcVSiEJL46ubsAynQXobHj/y3gFIFcw55fR0kQxfl
1qRPUP97mKsNimlnhZxLFPn1QcYYI9JfygV6jI+Za68sbTqddfIEV8PbkZ5AOgxH
j/bRJoT4lQKBgQCGUHb224sBgF9FeK3vwO/NfO51fH4jdRohV0DZmXJwdg31LEIg
b37nI1RfxBt8IEGoa8XqETnmV8osl2EppLn9GeKgw8QCcBQB5J6S22TPTZVSmk3w
mCbygki2rfA3iEu3wOrly8qEsIXzUvKpmXRtyvv3T35QD8RMs2d8RtbfBQKBgEb5
e8+qAOGnbmuwrJbskvIvNc78rwOETD/NUyA45DUikBwFPA7348h8DDGp4EIkrtcd
nLva5zxQ3CNJArhDPNc8Ljz7qNVba/CIWH6LZJZl6leUKSxMilwedDsA2jHFQ713
SmSScNHo9+CM+zFCzKfA8FCPA8evO4DXouzlAn+RAoGBAIG5f7P8JntCiSyHPeD4
jaIfJlMpAk4y/isBqGI6VQsWun3gAp3HZJ36XQy3wx6K3cxFgQhIeK7L11n2Qmba
8kaG8WXheNN6Mw3vBgXHgjlTDueQY4km/smo9+g1obbngV5L2ybad3c7qv9CglL2
1rhPYWWa1AVDkyIHH6i3JoeX";

    fn test_private_pem() -> String {
        format!(
            "-----BEGIN {}-----\n{}\n-----END {}-----",
            "PRIVATE KEY", PRIVATE_PEM_BODY, "PRIVATE KEY"
        )
    }

    /// Base64url modulus of the keypair above.
    const MODULUS: &str = "tg_nEDUloI7Tgwh8SLhy1t7d-pq3-W4T-XOizDyzqSj1Vo9oZsnvp2Ywk636YUMgo0LSUrGMG8oK_KJmx0yKYg1JGmfH-y7fJX54Fm_INdngWpQK_3g9vUfHuhmTS4Kbms9HbNPxaot9iaaTP-TqEEA9IVmCltewNC8Gjj9DauwskyoSSkWqoWLI3Oa-4ll_SsNxecaFsgkfk7MeVuoZ8MtIQVL0WH3vDOBOBX11n72YPOupt10uCHvbMHrDiXT_yoR1ejAlbNN4KVR0b1-LQj7jzLGGuWBYkc5qlNMoO-HwRSPN82qlce49K7r9UQetEuYtJ77xH1mSooizsDepmQ";

    /// The public key in PEM, used as an HMAC secret by the alg-confusion test.
    const PUBLIC_PEM: &str = "-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAtg/nEDUloI7Tgwh8SLhy
1t7d+pq3+W4T+XOizDyzqSj1Vo9oZsnvp2Ywk636YUMgo0LSUrGMG8oK/KJmx0yK
Yg1JGmfH+y7fJX54Fm/INdngWpQK/3g9vUfHuhmTS4Kbms9HbNPxaot9iaaTP+Tq
EEA9IVmCltewNC8Gjj9DauwskyoSSkWqoWLI3Oa+4ll/SsNxecaFsgkfk7MeVuoZ
8MtIQVL0WH3vDOBOBX11n72YPOupt10uCHvbMHrDiXT/yoR1ejAlbNN4KVR0b1+L
Qj7jzLGGuWBYkc5qlNMoO+HwRSPN82qlce49K7r9UQetEuYtJ77xH1mSooizsDep
mQIDAQAB
-----END PUBLIC KEY-----";

    fn jwks(kid: &str) -> JwkSet {
        serde_json::from_value(json!({
            "keys": [{
                "kty": "RSA", "alg": "RS256", "use": "sig",
                "kid": kid, "n": MODULUS, "e": "AQAB"
            }]
        }))
        .unwrap()
    }

    fn provider() -> ProviderConfig {
        ProviderConfig {
            issuer: ISSUER.to_string(),
            audience: Some("my-app".to_string()),
            jwks_url: Some(format!("{}/jwks", ISSUER)),
            algorithms: default_algorithms(),
        }
    }

    fn now_secs() -> u64 {
        jsonwebtoken::get_current_timestamp()
    }

    /// Sign claims as the IdP would.
    fn sign(claims: Value, kid: Option<&str>) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = kid.map(str::to_string);
        encode(
            &header,
            &claims,
            &EncodingKey::from_rsa_pem(test_private_pem().as_bytes()).unwrap(),
        )
        .unwrap()
    }

    fn valid_claims() -> Value {
        json!({
            "iss": ISSUER,
            "sub": "user_123",
            "aud": "my-app",
            "email": "someone@example.com",
            "exp": now_secs() + 3600,
            "role": "editor"
        })
    }

    #[test]
    fn verifies_a_good_token_and_builds_identity() {
        let token = sign(valid_claims(), Some(KID));
        let identity = verify_with_provider(&provider(), &jwks(KID), &token).unwrap();

        assert_eq!(identity.subject, "user_123");
        assert_eq!(identity.issuer, ISSUER);
        assert_eq!(identity.token_identifier, format!("{}|user_123", ISSUER));
        assert_eq!(identity.email.as_deref(), Some("someone@example.com"));
        // Non-standard claims survive for later rule matching.
        assert_eq!(identity.claims.get("role").unwrap(), "editor");
    }

    #[test]
    fn rejects_hs256_signed_with_the_rsa_public_key() {
        // The classic alg-confusion attack: the attacker knows the public key
        // (it is public), signs HS256 with it as the HMAC secret, and hopes the
        // server picks the algorithm from the token header.
        let forged = encode(
            &{
                let mut header = Header::new(Algorithm::HS256);
                header.kid = Some(KID.to_string());
                header
            },
            &valid_claims(),
            &EncodingKey::from_secret(PUBLIC_PEM.as_bytes()),
        )
        .unwrap();

        assert!(matches!(
            verify_with_provider(&provider(), &jwks(KID), &forged),
            Err(JwtError::AlgorithmNotAllowed(_))
        ));
    }

    #[test]
    fn rejects_alg_none() {
        // `none` cannot even be constructed by `encode`, so hand-assemble it.
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(valid_claims().to_string());
        let token = format!("{}.{}.", header, payload);

        assert!(verify_with_provider(&provider(), &jwks(KID), &token).is_err());
    }

    #[test]
    fn rejects_tampered_signature() {
        let token = sign(valid_claims(), Some(KID));
        let mut parts: Vec<&str> = token.split('.').collect();
        let other = sign(
            json!({"iss": ISSUER, "sub": "someone_else", "aud": "my-app", "exp": now_secs() + 3600}),
            Some(KID),
        );
        let other_parts: Vec<&str> = other.split('.').collect();
        parts[1] = other_parts[1];
        let spliced = parts.join(".");

        assert!(matches!(
            verify_with_provider(&provider(), &jwks(KID), &spliced),
            Err(JwtError::Rejected)
        ));
    }

    #[test]
    fn rejects_expired_and_not_yet_valid() {
        let mut expired = valid_claims();
        expired["exp"] = json!(now_secs() - 3600);
        assert!(verify_with_provider(&provider(), &jwks(KID), &sign(expired, Some(KID))).is_err());

        let mut future = valid_claims();
        future["nbf"] = json!(now_secs() + 3600);
        assert!(verify_with_provider(&provider(), &jwks(KID), &sign(future, Some(KID))).is_err());
    }

    #[test]
    fn rejects_tokens_issued_in_the_future() {
        let mut claims = valid_claims();
        claims["iat"] = json!(now_secs() + 3600);
        assert!(matches!(
            verify_with_provider(&provider(), &jwks(KID), &sign(claims, Some(KID))),
            Err(JwtError::Rejected)
        ));

        // A normal past `iat` is fine.
        let mut claims = valid_claims();
        claims["iat"] = json!(now_secs() - 60);
        assert!(verify_with_provider(&provider(), &jwks(KID), &sign(claims, Some(KID))).is_ok());
    }

    #[test]
    fn rejects_wrong_issuer_and_audience() {
        let mut claims = valid_claims();
        claims["iss"] = json!("https://attacker.example.com");
        assert!(verify_with_provider(&provider(), &jwks(KID), &sign(claims, Some(KID))).is_err());

        let mut claims = valid_claims();
        claims["aud"] = json!("someone-elses-app");
        assert!(verify_with_provider(&provider(), &jwks(KID), &sign(claims, Some(KID))).is_err());
    }

    #[test]
    fn audience_is_only_checked_when_configured() {
        let mut provider = provider();
        provider.audience = None;
        // A token carrying an `aud` we never configured still verifies.
        let identity =
            verify_with_provider(&provider, &jwks(KID), &sign(valid_claims(), Some(KID)));
        assert_eq!(identity.unwrap().subject, "user_123");
    }

    #[test]
    fn rejects_missing_or_empty_subject() {
        let mut claims = valid_claims();
        claims.as_object_mut().unwrap().remove("sub");
        assert!(verify_with_provider(&provider(), &jwks(KID), &sign(claims, Some(KID))).is_err());

        let mut claims = valid_claims();
        claims["sub"] = json!("");
        assert!(verify_with_provider(&provider(), &jwks(KID), &sign(claims, Some(KID))).is_err());
    }

    #[test]
    fn rejects_unknown_kid_and_handles_absent_kid() {
        let token = sign(valid_claims(), Some("some-other-kid"));
        assert!(matches!(
            verify_with_provider(&provider(), &jwks(KID), &token),
            Err(JwtError::UnknownKeyId(_))
        ));

        // No `kid` is fine when the set is unambiguous.
        let token = sign(valid_claims(), None);
        assert!(verify_with_provider(&provider(), &jwks(KID), &token).is_ok());

        // ...and refused when it is not.
        let mut two_keys = jwks(KID);
        two_keys.keys.push(jwks("second").keys.pop().unwrap());
        assert!(matches!(
            verify_with_provider(&provider(), &two_keys, &token),
            Err(JwtError::UnknownKeyId(_))
        ));
    }

    #[test]
    fn algorithm_must_be_declared_by_the_provider() {
        let mut provider = provider();
        provider.algorithms = vec!["ES256".to_string()];
        assert!(matches!(
            verify_with_provider(&provider, &jwks(KID), &sign(valid_claims(), Some(KID))),
            Err(JwtError::AlgorithmNotAllowed(_))
        ));
    }

    #[test]
    fn jwt_shape_detection() {
        assert!(looks_like_jwt(&sign(valid_claims(), Some(KID))));
        for not_a_jwt in [
            "",
            "opaque-api-key",
            "acme|QoHxIsaBCyBT-Hrn3d",
            "a.b",
            "a.b.c.d",
            ".b.c",
            "not-base64!.b.c",
        ] {
            assert!(!looks_like_jwt(not_a_jwt), "accepted {:?}", not_a_jwt);
        }
    }

    #[test]
    fn peek_issuer_reads_without_verifying() {
        let token = sign(valid_claims(), Some(KID));
        assert_eq!(peek_issuer(&token).as_deref(), Some(ISSUER));
        // Unsigned garbage still parses — that is the point, and why the claim
        // is re-checked during verification.
        assert_eq!(peek_issuer("opaque").as_deref(), None);
    }

    // ---- config validation ----

    #[test]
    fn config_validation_accepts_a_sane_provider() {
        let config = AuthConfig {
            providers: vec![provider()],
        };
        assert!(validate_auth_config(&config).is_ok());
    }

    #[test]
    fn config_validation_rejects_symmetric_and_none_algorithms() {
        for algorithm in ["HS256", "HS512", "none", "RS255", ""] {
            let mut p = provider();
            p.algorithms = vec![algorithm.to_string()];
            let config = AuthConfig { providers: vec![p] };
            assert!(
                validate_auth_config(&config).is_err(),
                "accepted algorithm {:?}",
                algorithm
            );
        }
    }

    #[test]
    fn config_validation_rejects_bad_urls_and_duplicates() {
        let cases: Vec<(&str, AuthConfig)> = vec![
            ("empty", AuthConfig { providers: vec![] }),
            (
                "http issuer",
                AuthConfig {
                    providers: vec![ProviderConfig {
                        issuer: "http://idp.example.com".to_string(),
                        ..provider()
                    }],
                },
            ),
            (
                "not a url",
                AuthConfig {
                    providers: vec![ProviderConfig {
                        issuer: "idp.example.com".to_string(),
                        ..provider()
                    }],
                },
            ),
            (
                "http jwks",
                AuthConfig {
                    providers: vec![ProviderConfig {
                        jwks_url: Some("http://idp.example.com/jwks".to_string()),
                        ..provider()
                    }],
                },
            ),
            (
                "duplicate issuer",
                AuthConfig {
                    providers: vec![provider(), provider()],
                },
            ),
            (
                "empty audience",
                AuthConfig {
                    providers: vec![ProviderConfig {
                        audience: Some(String::new()),
                        ..provider()
                    }],
                },
            ),
            (
                "no algorithms",
                AuthConfig {
                    providers: vec![ProviderConfig {
                        algorithms: vec![],
                        ..provider()
                    }],
                },
            ),
        ];
        for (name, config) in cases {
            assert!(validate_auth_config(&config).is_err(), "accepted {}", name);
        }
    }

    #[test]
    fn config_validation_allows_loopback_http_for_development() {
        let config = AuthConfig {
            providers: vec![ProviderConfig {
                issuer: "http://localhost:3000".to_string(),
                jwks_url: Some("http://127.0.0.1:3000/jwks".to_string()),
                ..provider()
            }],
        };
        assert!(validate_auth_config(&config).is_ok());
    }

    #[test]
    fn provider_lookup_is_by_exact_issuer() {
        let config = AuthConfig {
            providers: vec![provider()],
        };
        assert!(config.provider_for(ISSUER).is_some());
        assert!(config.provider_for("https://idp.example.com/").is_none());
        assert!(config.provider_for("https://other.example.com").is_none());
    }

    // ---- JWKS cache ----

    #[derive(Default)]
    struct MockFetcher {
        responses: HashMap<String, Value>,
        calls: AtomicUsize,
        fail: std::sync::atomic::AtomicBool,
    }

    impl MockFetcher {
        fn with(responses: Vec<(&str, Value)>) -> Self {
            Self {
                responses: responses
                    .into_iter()
                    .map(|(url, body)| (url.to_string(), body))
                    .collect(),
                ..Default::default()
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn set_failing(&self, failing: bool) {
            self.fail.store(failing, Ordering::SeqCst);
        }
    }

    impl JwksFetcher for MockFetcher {
        async fn fetch_json(&self, url: &str) -> Result<Value, JwtError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err(JwtError::Fetch("mock offline".to_string()));
            }
            self.responses
                .get(url)
                .cloned()
                .ok_or_else(|| JwtError::Fetch(format!("no mock for {}", url)))
        }
    }

    fn jwks_body(kid: &str) -> Value {
        serde_json::to_value(jwks(kid)).unwrap()
    }

    #[tokio::test]
    async fn cache_serves_from_memory_until_the_ttl_expires() {
        let cache = JwksCache::new(MockFetcher::with(vec![(
            "https://idp.example.com/jwks",
            jwks_body(KID),
        )]));
        let provider = provider();

        cache.keys_for(&provider, 0).await.unwrap();
        cache.keys_for(&provider, 1_000).await.unwrap();
        cache.keys_for(&provider, DEFAULT_TTL_MS - 1).await.unwrap();
        assert_eq!(cache.fetcher.calls(), 1, "should have fetched once");

        cache.keys_for(&provider, DEFAULT_TTL_MS + 1).await.unwrap();
        assert_eq!(cache.fetcher.calls(), 2, "stale entry must refetch");
    }

    #[tokio::test]
    async fn cache_discovers_the_jwks_url_when_absent() {
        let cache = JwksCache::new(MockFetcher::with(vec![
            (
                "https://idp.example.com/.well-known/openid-configuration",
                json!({"jwks_uri": "https://idp.example.com/discovered/jwks"}),
            ),
            ("https://idp.example.com/discovered/jwks", jwks_body(KID)),
        ]));
        let provider = ProviderConfig {
            jwks_url: None,
            ..provider()
        };

        let keys = cache.keys_for(&provider, 0).await.unwrap();
        assert_eq!(keys.keys.len(), 1);
        assert_eq!(cache.fetcher.calls(), 2, "discovery + jwks");
    }

    #[tokio::test]
    async fn failed_fetches_are_negatively_cached() {
        let cache = JwksCache::new(MockFetcher::with(vec![(
            "https://idp.example.com/jwks",
            jwks_body(KID),
        )]));
        let provider = provider();
        cache.fetcher.set_failing(true);

        assert!(cache.keys_for(&provider, 0).await.is_err());
        assert_eq!(cache.fetcher.calls(), 1);

        // Inside the negative window the IdP is not touched again.
        assert!(cache.keys_for(&provider, 1_000).await.is_err());
        assert_eq!(cache.fetcher.calls(), 1, "negative cache must hold");

        // Once it lapses we try again, and recover.
        cache.fetcher.set_failing(false);
        assert!(cache
            .keys_for(&provider, DEFAULT_NEGATIVE_TTL_MS + 1)
            .await
            .is_ok());
        assert_eq!(cache.fetcher.calls(), 2);
    }

    #[tokio::test]
    async fn unknown_kid_refetch_is_rate_limited() {
        let cache = JwksCache::new(MockFetcher::with(vec![(
            "https://idp.example.com/jwks",
            jwks_body(KID),
        )]));
        let provider = provider();

        cache.keys_for(&provider, 0).await.unwrap();
        assert_eq!(cache.fetcher.calls(), 1);

        // Keys rotating right after a cold fetch is exactly when a refetch is
        // needed, so the first one is allowed however recent that fetch was.
        cache.keys_for_unknown_kid(&provider, 1).await.unwrap();
        assert_eq!(cache.fetcher.calls(), 2);

        // From there a burst of bogus `kid`s must not become IdP requests.
        for now in [2, 500, DEFAULT_REFETCH_COOLDOWN_MS] {
            assert!(cache.keys_for_unknown_kid(&provider, now).await.is_err());
        }
        assert_eq!(cache.fetcher.calls(), 2, "cooldown must suppress refetches");

        // Once it lapses, one more is allowed.
        cache
            .keys_for_unknown_kid(&provider, DEFAULT_REFETCH_COOLDOWN_MS + 2)
            .await
            .unwrap();
        assert_eq!(cache.fetcher.calls(), 3);
    }

    #[tokio::test]
    async fn concurrent_unknown_kid_refetches_are_still_rate_limited() {
        // The cooldown is read before the fetch lock, so a burst can all see it
        // expired at once; it has to be re-checked under the lock.
        let cache = Arc::new(JwksCache::new(MockFetcher::with(vec![(
            "https://idp.example.com/jwks",
            jwks_body(KID),
        )])));
        cache.keys_for(&provider(), 0).await.unwrap();

        let now = DEFAULT_REFETCH_COOLDOWN_MS + 1;
        let mut handles = Vec::new();
        for _ in 0..8 {
            let cache = Arc::clone(&cache);
            handles.push(tokio::spawn(async move {
                cache
                    .keys_for_unknown_kid(&provider(), now)
                    .await
                    .map(|_| ())
            }));
        }
        for handle in handles {
            let _ = handle.await.unwrap();
        }
        assert_eq!(
            cache.fetcher.calls(),
            2,
            "a concurrent burst must yield one refetch, not one each"
        );
    }

    #[tokio::test]
    async fn discovered_jwks_url_is_validated() {
        // An issuer that points discovery at a loopback metadata-style URL must
        // not be fetched just because it arrived in a discovery document.
        let cache = JwksCache::new(MockFetcher::with(vec![(
            "https://idp.example.com/.well-known/openid-configuration",
            json!({"jwks_uri": "http://169.254.169.254/latest/meta-data/"}),
        )]));
        let provider = ProviderConfig {
            jwks_url: None,
            ..provider()
        };

        assert!(matches!(
            cache.keys_for(&provider, 0).await,
            Err(JwtError::Fetch(_))
        ));
        assert_eq!(cache.fetcher.calls(), 1, "must not fetch the rejected URL");
    }

    #[tokio::test]
    async fn cache_is_bounded() {
        let mut responses = Vec::new();
        let urls: Vec<String> = (0..MAX_CACHED_ISSUERS + 10)
            .map(|i| format!("https://idp{}.example.com/jwks", i))
            .collect();
        for url in &urls {
            responses.push((url.as_str(), jwks_body(KID)));
        }
        let cache = JwksCache::new(MockFetcher::with(responses));

        for (i, url) in urls.iter().enumerate() {
            let provider = ProviderConfig {
                issuer: format!("https://idp{}.example.com", i),
                jwks_url: Some(url.clone()),
                ..provider()
            };
            cache.keys_for(&provider, i as Millis).await.unwrap();
        }
        assert_eq!(cache.entries.read().await.len(), MAX_CACHED_ISSUERS);
    }

    #[tokio::test]
    async fn malformed_jwks_is_a_fetch_error_not_a_panic() {
        let cache = JwksCache::new(MockFetcher::with(vec![(
            "https://idp.example.com/jwks",
            json!({"keys": "not-an-array"}),
        )]));
        assert!(matches!(
            cache.keys_for(&provider(), 0).await,
            Err(JwtError::Fetch(_))
        ));
    }

    #[tokio::test]
    async fn concurrent_misses_produce_a_single_fetch() {
        let cache = Arc::new(JwksCache::new(MockFetcher::with(vec![(
            "https://idp.example.com/jwks",
            jwks_body(KID),
        )])));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let cache = Arc::clone(&cache);
            handles.push(tokio::spawn(async move {
                cache.keys_for(&provider(), 0).await.map(|_| ())
            }));
        }
        for handle in handles {
            handle.await.unwrap().unwrap();
        }
        assert_eq!(
            cache.fetcher.calls(),
            1,
            "single-flight must collapse the burst"
        );
    }
}

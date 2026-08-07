use axum::{extract::Request, middleware::Next, response::Response};
use std::collections::HashMap;
use std::sync::Arc;

use crate::error::AppError;
use crate::handlers::AppState;
use crate::jwt::{self, Principal};

/// Parse `API_KEYS` env format: `key1:project1,key2:project2`.
/// A project of `*` grants access to all projects.
pub fn parse_api_keys(raw: &str) -> HashMap<String, String> {
    raw.split(',')
        .filter_map(|pair| {
            let (key, project) = pair.trim().split_once(':')?;
            if key.is_empty() || project.is_empty() {
                return None;
            }
            Some((key.to_string(), project.to_string()))
        })
        .collect()
}

/// Resolve a bearer token to the project an API key grants. Static `API_KEYS`
/// entries are checked first; failing that the token is tried as an
/// instance-derived key, so both kinds work side by side.
fn resolve_api_key(state: &AppState, token: &str) -> Option<String> {
    if let Some(project) = state.api_keys.get(token) {
        return Some(project.clone());
    }

    let broker = state.keybroker.as_ref()?;
    match broker.verify(token) {
        Ok(claims) => Some(claims.project),
        Err(e) => {
            // Debug, not warn: unauthenticated traffic must not fill the logs.
            tracing::debug!("rejected instance key: {}", e);
            None
        }
    }
}

/// Opaque bearer values are API-key candidates only. They must not enter the
/// end-user lookup path, which can read the schema and contact a provider.
fn require_jwt_shape(token: &str) -> Result<(), AppError> {
    jwt::looks_like_jwt(token)
        .then_some(())
        .ok_or(AppError::Unauthorized)
}

/// Verify a JWT against the project's configured providers.
async fn resolve_end_user(
    state: &AppState,
    project: &str,
    token: &str,
) -> Result<Principal, AppError> {
    let schema = state.db.get_schema(project).await?;
    let auth = schema
        .as_ref()
        .and_then(|s| s.auth.as_ref())
        .ok_or(AppError::Unauthorized)?;

    // The unverified `iss` only selects which provider's keys to try; it is
    // checked again against that provider during verification.
    let issuer = jwt::peek_issuer(token).ok_or(AppError::Unauthorized)?;
    let provider = auth.provider_for(&issuer).ok_or(AppError::Unauthorized)?;

    let now = jwt::now_ms();
    let keys = state.jwks.keys_for(provider, now).await.map_err(|e| {
        // Operators need to see an unreachable IdP; the caller gets a flat
        // 401 rather than a report on our infrastructure.
        tracing::warn!("jwks unavailable for {}: {}", provider.issuer, e);
        AppError::Unauthorized
    })?;

    match jwt::verify_with_provider(provider, &keys, token) {
        Ok(identity) => Ok(Principal::EndUser(identity)),
        // An unknown `kid` is what key rotation looks like: refetch once
        // (rate-limited inside the cache) and retry before giving up.
        Err(jwt::JwtError::UnknownKeyId(_)) => {
            let keys = state
                .jwks
                .keys_for_unknown_kid(provider, now)
                .await
                .map_err(|_| AppError::Unauthorized)?;
            jwt::verify_with_provider(provider, &keys, token)
                .map(Principal::EndUser)
                .map_err(|e| {
                    tracing::debug!("rejected token after key refresh: {}", e);
                    AppError::Unauthorized
                })
        }
        Err(e) => {
            tracing::debug!("rejected token: {}", e);
            Err(AppError::Unauthorized)
        }
    }
}

/// Authenticate `/v1/{project}/...` requests. `/health` is not under `/v1`.
///
/// This layer only establishes *who* is calling and inserts a [`Principal`] for
/// the handlers; authorization needs the collection and often the document, so
/// it lives in the handlers behind `rules::`. API-key project scoping stays
/// here because it needs nothing else.
///
/// A request with no credential becomes [`Principal::Anonymous`] and is passed
/// through — `"public"` rules cannot work otherwise. A credential that is
/// present but does not verify is still a 401: presenting a broken token is an
/// error, not anonymity.
pub async fn require_auth(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let path = req.uri().path();

    let Some(rest) = path.strip_prefix("/v1/") else {
        return Ok(next.run(req).await);
    };
    let project = rest.split('/').next().unwrap_or("").to_string();

    let token = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);

    let principal = match token {
        None => Principal::Anonymous,
        Some(token) => match resolve_api_key(&state, &token) {
            Some(granted) => {
                if granted != "*" && granted != project {
                    return Err(AppError::Forbidden);
                }
                Principal::ApiKey { project: granted }
            }
            None => {
                require_jwt_shape(&token)?;
                resolve_end_user(&state, &project, &token).await?
            }
        },
    };

    req.extensions_mut().insert(principal);
    Ok(next.run(req).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_parser_keeps_only_complete_entries() {
        let parsed = parse_api_keys("key-a:project-a,key-b:*, :project,key:,missing-colon,:");
        assert_eq!(
            parsed,
            HashMap::from([
                ("key-a".to_string(), "project-a".to_string()),
                ("key-b".to_string(), "*".to_string()),
            ])
        );
    }

    #[test]
    fn only_jwt_shaped_bearers_reach_end_user_authentication() {
        assert!(require_jwt_shape("eyJhbGciOiJSUzI1NiJ9.payload.signature").is_ok());
        for opaque in ["opaque", "a.b", "bad.payload.signature"] {
            assert!(matches!(
                require_jwt_shape(opaque),
                Err(AppError::Unauthorized)
            ));
        }
    }
}

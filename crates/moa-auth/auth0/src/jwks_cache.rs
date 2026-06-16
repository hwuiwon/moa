//! In-memory JWKS cache for JWT signature validation.
//!
//! The cache refreshes on `kid` miss and after its configured TTL. It stores
//! only public decoding keys and is safe to clone across request handlers.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use jsonwebtoken::{DecodingKey, jwk::JwkSet};
use moka::future::Cache;
use thiserror::Error;

/// Decoding keys from one JWKS fetch, cached as a single unit so that a `kid`
/// rotation refreshes every key together rather than per entry.
type KeySet = Arc<HashMap<String, DecodingKey>>;

/// Thread-safe cache of public JWT decoding keys fetched from one JWKS URL.
#[derive(Clone)]
pub struct JwksCache {
    keys: Cache<(), KeySet>,
    jwks_url: String,
    http: reqwest::Client,
}

impl JwksCache {
    /// Create a JWKS cache for `jwks_url` with entries valid for `ttl`.
    #[must_use]
    pub fn new(jwks_url: impl Into<String>, ttl: Duration) -> Self {
        Self {
            keys: Cache::builder().time_to_live(ttl).build(),
            jwks_url: jwks_url.into(),
            http: reqwest::Client::new(),
        }
    }

    /// Return the decoding key for `kid`, refreshing the cache when needed.
    pub async fn key_for(&self, kid: &str) -> Result<DecodingKey, JwksError> {
        // Fast path: an unexpired key set that already contains `kid`.
        if let Some(keys) = self.keys.get(&()).await
            && let Some(key) = keys.get(kid)
        {
            return Ok(key.clone());
        }

        // Slow path: cold cache, expired TTL, or a `kid` that may have rotated
        // in. Drop any stale set and refetch the whole JWKS (coalescing
        // concurrent refreshes) before looking `kid` up again.
        self.keys.invalidate(&()).await;
        self.keys
            .try_get_with((), self.fetch_keys())
            .await
            .map_err(|error: Arc<JwksError>| (*error).clone())?
            .get(kid)
            .cloned()
            .ok_or_else(|| JwksError::UnknownKid(kid.to_string()))
    }

    async fn fetch_keys(&self) -> Result<KeySet, JwksError> {
        tracing::info!(jwks_url = %self.jwks_url, "refreshing JWKS");
        let jwks: JwkSet = self
            .http
            .get(&self.jwks_url)
            .send()
            .await
            .map_err(|error| JwksError::Http(error.to_string()))?
            .error_for_status()
            .map_err(|error| JwksError::Http(error.to_string()))?
            .json()
            .await
            .map_err(|error| JwksError::Parse(error.to_string()))?;

        let mut keys = HashMap::new();
        for jwk in jwks.keys {
            let Some(kid) = jwk.common.key_id.clone() else {
                continue;
            };
            match DecodingKey::from_jwk(&jwk) {
                Ok(key) => {
                    keys.insert(kid, key);
                }
                Err(error) => {
                    tracing::warn!(?error, "skipping unsupported JWK");
                }
            }
        }

        tracing::info!(count = keys.len(), "JWKS cache refreshed");
        Ok(Arc::new(keys))
    }
}

/// JWKS retrieval and key-selection failures.
#[derive(Debug, Clone, Error)]
pub enum JwksError {
    /// The JWKS endpoint could not be fetched successfully.
    #[error("JWKS HTTP error: {0}")]
    Http(String),
    /// The JWKS body could not be parsed.
    #[error("JWKS parse error: {0}")]
    Parse(String),
    /// No key matching the JWT header `kid` exists in the refreshed JWKS.
    #[error("unknown kid: {0}")]
    UnknownKid(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use httpmock::{Method::GET, MockServer};
    use rsa::traits::PublicKeyParts;
    use rsa::{RsaPrivateKey, rand_core::OsRng};

    /// Builds a one-key JWKS document for `kid` backed by a fresh RSA key.
    fn jwks_json(kid: &str) -> serde_json::Value {
        let key = RsaPrivateKey::new(&mut OsRng, 2048).expect("generate RSA test key");
        let public = key.to_public_key();
        serde_json::json!({
            "keys": [{
                "kty": "RSA",
                "kid": kid,
                "use": "sig",
                "alg": "RS256",
                "n": URL_SAFE_NO_PAD.encode(public.n().to_bytes_be()),
                "e": URL_SAFE_NO_PAD.encode(public.e().to_bytes_be()),
            }]
        })
    }

    #[tokio::test]
    async fn caches_within_ttl_and_refetches_whole_jwks_on_unknown_kid() {
        // Pins: a `kid` hit inside the TTL is served from cache without a refetch,
        // while an unknown `kid` forces a single whole-JWKS refresh (rotation).
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/jwks.json");
                then.status(200).json_body(jwks_json("kid-1"));
            })
            .await;

        let cache = JwksCache::new(server.url("/jwks.json"), Duration::from_secs(3600));

        cache.key_for("kid-1").await.expect("known kid resolves");
        assert_eq!(mock.hits_async().await, 1, "first lookup fetches the JWKS");

        cache
            .key_for("kid-1")
            .await
            .expect("known kid resolves from cache");
        assert_eq!(
            mock.hits_async().await,
            1,
            "second lookup is served from cache"
        );

        assert!(
            matches!(cache.key_for("kid-2").await, Err(JwksError::UnknownKid(_))),
            "unknown kid is rejected"
        );
        assert_eq!(
            mock.hits_async().await,
            2,
            "unknown kid forces a whole-JWKS refresh"
        );
    }
}

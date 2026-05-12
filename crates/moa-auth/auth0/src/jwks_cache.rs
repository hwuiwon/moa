//! In-memory JWKS cache for JWT signature validation.
//!
//! The cache refreshes on `kid` miss and after its configured TTL. It stores
//! only public decoding keys and is safe to clone across request handlers.

use jsonwebtoken::{DecodingKey, jwk::JwkSet};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::RwLock;

/// Thread-safe cache of public JWT decoding keys fetched from one JWKS URL.
#[derive(Clone)]
pub struct JwksCache {
    inner: Arc<RwLock<CacheState>>,
    jwks_url: String,
    http: reqwest::Client,
    ttl: Duration,
}

struct CacheState {
    keys: HashMap<String, DecodingKey>,
    fetched_at: Option<Instant>,
}

impl JwksCache {
    /// Create a JWKS cache for `jwks_url` with entries valid for `ttl`.
    #[must_use]
    pub fn new(jwks_url: impl Into<String>, ttl: Duration) -> Self {
        Self {
            inner: Arc::new(RwLock::new(CacheState {
                keys: HashMap::new(),
                fetched_at: None,
            })),
            jwks_url: jwks_url.into(),
            http: reqwest::Client::new(),
            ttl,
        }
    }

    /// Return the decoding key for `kid`, refreshing the cache when needed.
    pub async fn key_for(&self, kid: &str) -> Result<DecodingKey, JwksError> {
        {
            let state = self.inner.read().await;
            if let Some(key) = state.keys.get(kid)
                && state
                    .fetched_at
                    .is_some_and(|fetched| fetched.elapsed() < self.ttl)
            {
                return Ok(key.clone());
            }
        }

        self.refresh().await?;
        let state = self.inner.read().await;
        state
            .keys
            .get(kid)
            .cloned()
            .ok_or_else(|| JwksError::UnknownKid(kid.to_string()))
    }

    async fn refresh(&self) -> Result<(), JwksError> {
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

        let mut state = self.inner.write().await;
        state.keys = keys;
        state.fetched_at = Some(Instant::now());
        tracing::info!(count = state.keys.len(), "JWKS cache refreshed");
        Ok(())
    }
}

/// JWKS retrieval and key-selection failures.
#[derive(Debug, Error)]
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

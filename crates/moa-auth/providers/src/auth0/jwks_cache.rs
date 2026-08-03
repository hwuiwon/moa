//! In-memory JWKS cache for JWT signature validation.
//!
//! The cache serves known `kid`s from a last-known-good key set and refreshes
//! that set — never discarding it — when a `kid` is unknown or its keys age past
//! the configured TTL. It stores only public decoding keys and is safe to clone
//! across request handlers.
//!
//! Because the JWT header (and therefore its `kid`) is attacker-controllable
//! before signature validation, refreshes are hardened against amplification:
//!
//! * A miss never evicts the last-known-good set; the set is replaced only when
//!   a refresh succeeds, so known keys keep validating during a provider outage.
//! * A single-flight refresh gate collapses concurrent misses into one fetch and
//!   enforces a cooldown, so a flood of distinct random `kid`s cannot drive one
//!   outbound JWKS request each.
//! * Unknown `kid`s confirmed absent by a successful refresh are negative-cached
//!   for a short, bounded window, so repeated probes of the same `kid` do not
//!   refetch.
//! * The JWKS HTTP client carries tight connect/request/body-size budgets.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jsonwebtoken::{DecodingKey, jwk::JwkSet};
use moka::future::Cache;
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};

/// Decoding keys from one JWKS fetch, cached as a single unit so that a `kid`
/// rotation refreshes every key together rather than per entry.
type KeySet = Arc<HashMap<String, DecodingKey>>;

/// Connect-phase budget for one JWKS fetch.
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Total wall-clock budget for one JWKS fetch (connect, send, and read).
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Maximum accepted JWKS response body. Real documents are a few kilobytes; this
/// bounds memory against a misbehaving or compromised provider endpoint.
const MAX_JWKS_BODY_BYTES: u64 = 1024 * 1024;
/// Minimum interval between JWKS refresh attempts. Rate-limits refreshes so that
/// a burst of distinct unknown `kid`s cannot amplify into one fetch each.
const DEFAULT_REFRESH_COOLDOWN: Duration = Duration::from_secs(10);
/// How long a `kid` confirmed absent by a successful refresh is remembered as
/// unknown so repeated probes are rejected without refetching.
const DEFAULT_NEGATIVE_TTL: Duration = Duration::from_secs(60);
/// Bound on the negative cache; the keyspace is attacker-controlled.
const DEFAULT_NEGATIVE_CAPACITY: u64 = 1024;

/// Mutable last-known-good state guarded by [`JwksCache::state`].
#[derive(Default)]
struct KeyState {
    /// Most recent successfully fetched key set (empty before the first fetch).
    keys: KeySet,
    /// When [`Self::keys`] was last successfully refreshed; drives TTL freshness.
    refreshed_at: Option<Instant>,
    /// When a refresh was last attempted (success or failure); drives the
    /// cooldown so a failing provider is not hammered.
    last_attempt: Option<Instant>,
}

impl KeyState {
    /// Whether the current key set is within its freshness TTL.
    fn is_fresh(&self, ttl: Duration) -> bool {
        self.refreshed_at
            .is_some_and(|refreshed| refreshed.elapsed() < ttl)
    }

    /// Whether a refresh was attempted within the cooldown window.
    fn within_cooldown(&self, cooldown: Duration) -> bool {
        self.last_attempt
            .is_some_and(|attempt| attempt.elapsed() < cooldown)
    }
}

/// Thread-safe cache of public JWT decoding keys fetched from one JWKS URL.
#[derive(Clone)]
pub struct JwksCache {
    inner: Arc<Inner>,
}

struct Inner {
    jwks_url: String,
    http: reqwest::Client,
    /// Last-known-good key set and refresh bookkeeping.
    state: RwLock<KeyState>,
    /// Single-flight refresh gate: held across the miss re-check, cooldown
    /// decision, fetch, and replace so concurrent misses collapse into one
    /// fetch and observe each other's cooldown.
    refresh: Mutex<()>,
    /// Bounded, short-TTL memory of `kid`s a successful refresh proved absent.
    negative: Cache<String, ()>,
    ttl: Duration,
    cooldown: Duration,
}

impl JwksCache {
    /// Create a JWKS cache for `jwks_url` whose keys are considered fresh for
    /// `ttl`, with default refresh cooldown and negative-cache policy.
    #[must_use]
    pub fn new(jwks_url: impl Into<String>, ttl: Duration) -> Self {
        Self::with_policy(
            jwks_url,
            ttl,
            DEFAULT_REFRESH_COOLDOWN,
            DEFAULT_NEGATIVE_TTL,
            DEFAULT_NEGATIVE_CAPACITY,
        )
    }

    /// Create a JWKS cache with explicit refresh cooldown and negative-cache
    /// policy. Exposed for tests that must exercise cooldown and negative-TTL
    /// windows without real-time waits.
    #[must_use]
    fn with_policy(
        jwks_url: impl Into<String>,
        ttl: Duration,
        cooldown: Duration,
        negative_ttl: Duration,
        negative_capacity: u64,
    ) -> Self {
        // Tight budgets: an unverified attacker-supplied `kid` can drive a fetch,
        // so a slow or hostile endpoint must not tie up an auth request.
        let http = reqwest::Client::builder()
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .timeout(HTTP_REQUEST_TIMEOUT)
            .build()
            .unwrap_or_else(|error| {
                tracing::warn!(?error, "falling back to default JWKS HTTP client");
                reqwest::Client::new()
            });
        Self {
            inner: Arc::new(Inner {
                jwks_url: jwks_url.into(),
                http,
                state: RwLock::new(KeyState::default()),
                refresh: Mutex::new(()),
                negative: Cache::builder()
                    .max_capacity(negative_capacity)
                    .time_to_live(negative_ttl)
                    .build(),
                ttl,
                cooldown,
            }),
        }
    }

    /// Return the decoding key for `kid`.
    ///
    /// Serves a fresh known `kid` from cache without any network call. Otherwise
    /// takes the single-flight refresh path, which — subject to the cooldown —
    /// refreshes the key set and either resolves `kid`, serves a still-valid
    /// last-known-good key during a provider outage, negative-caches a confirmed
    /// unknown `kid` ([`JwksError::UnknownKid`]), or surfaces the provider
    /// failure ([`JwksError::Http`]/[`JwksError::Parse`]) without discarding
    /// valid keys.
    pub async fn key_for(&self, kid: &str) -> Result<DecodingKey, JwksError> {
        let inner = self.inner.as_ref();

        // Fast path: a known `kid` whose key set is still within its TTL.
        if let Some(key) = inner.fresh_known_key(kid).await {
            return Ok(key);
        }
        // A `kid` a recent successful refresh proved absent: reject without a
        // fetch. Checked before taking the refresh gate so a repeated probe is
        // cheap.
        if inner.negative.get(kid).await.is_some() {
            return Err(JwksError::UnknownKid(kid.to_string()));
        }

        // Slow path: single-flight. Concurrent misses queue here so at most one
        // fetch runs; each re-checks the state the winner just published.
        let _guard = inner.refresh.lock().await;

        if let Some(key) = inner.fresh_known_key(kid).await {
            return Ok(key);
        }
        if inner.negative.get(kid).await.is_some() {
            return Err(JwksError::UnknownKid(kid.to_string()));
        }

        if inner.state.read().await.within_cooldown(inner.cooldown) {
            // Rate-limited: no fetch. Serve a stale-but-known key (availability)
            // or reject an unknown `kid` as an invalid credential. A `kid` not
            // fetched here is not negative-cached — only a successful refresh may
            // confirm absence.
            return inner
                .known_key(kid)
                .await
                .ok_or_else(|| JwksError::UnknownKid(kid.to_string()));
        }

        inner.state.write().await.last_attempt = Some(Instant::now());
        match inner.fetch_keys().await {
            Ok(new_keys) => {
                {
                    let mut state = inner.state.write().await;
                    state.keys = new_keys.clone();
                    state.refreshed_at = Some(Instant::now());
                }
                match new_keys.get(kid) {
                    Some(key) => Ok(key.clone()),
                    None => {
                        inner.negative.insert(kid.to_string(), ()).await;
                        Err(JwksError::UnknownKid(kid.to_string()))
                    }
                }
            }
            // Provider failure must not discard still-valid keys: serve the
            // last-known-good key for `kid` if we have one, else surface the
            // provider error.
            Err(error) => inner.known_key(kid).await.ok_or(error),
        }
    }
}

impl Inner {
    /// Returns the decoding key for `kid` only if the key set is fresh.
    async fn fresh_known_key(&self, kid: &str) -> Option<DecodingKey> {
        let state = self.state.read().await;
        state
            .is_fresh(self.ttl)
            .then(|| state.keys.get(kid).cloned())
            .flatten()
    }

    /// Returns the decoding key for `kid` from the last-known-good set,
    /// regardless of freshness.
    async fn known_key(&self, kid: &str) -> Option<DecodingKey> {
        self.state.read().await.keys.get(kid).cloned()
    }

    async fn fetch_keys(&self) -> Result<KeySet, JwksError> {
        tracing::info!(jwks_url = %self.jwks_url, "refreshing JWKS");
        let response = self
            .http
            .get(&self.jwks_url)
            .send()
            .await
            .map_err(|error| JwksError::Http(error.to_string()))?
            .error_for_status()
            .map_err(|error| JwksError::Http(error.to_string()))?;

        // Body-size budget: reject an over-large declared length before reading,
        // and cap the actually-read body as a backstop.
        if response
            .content_length()
            .is_some_and(|len| len > MAX_JWKS_BODY_BYTES)
        {
            return Err(JwksError::Http(format!(
                "JWKS body exceeds {MAX_JWKS_BODY_BYTES} byte budget"
            )));
        }
        let body = response
            .bytes()
            .await
            .map_err(|error| JwksError::Http(error.to_string()))?;
        if body.len() as u64 > MAX_JWKS_BODY_BYTES {
            return Err(JwksError::Http(format!(
                "JWKS body exceeds {MAX_JWKS_BODY_BYTES} byte budget"
            )));
        }
        let jwks: JwkSet =
            serde_json::from_slice(&body).map_err(|error| JwksError::Parse(error.to_string()))?;

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

    /// Builds one JWK entry for `kid` backed by a fresh RSA key.
    fn jwk_entry(kid: &str) -> serde_json::Value {
        let key = RsaPrivateKey::new(&mut OsRng, 2048).expect("generate RSA test key");
        let public = key.to_public_key();
        serde_json::json!({
            "kty": "RSA",
            "kid": kid,
            "use": "sig",
            "alg": "RS256",
            "n": URL_SAFE_NO_PAD.encode(public.n().to_bytes_be()),
            "e": URL_SAFE_NO_PAD.encode(public.e().to_bytes_be()),
        })
    }

    /// Builds a JWKS document covering every `kid` in `kids`.
    fn jwks_body(kids: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "keys": kids.iter().map(|kid| jwk_entry(kid)).collect::<Vec<_>>(),
        })
    }

    /// JWKS cache with explicit cooldown/negative windows for deterministic
    /// timing without real-time waits.
    fn cache_with(
        url: String,
        ttl: Duration,
        cooldown: Duration,
        negative_ttl: Duration,
    ) -> JwksCache {
        JwksCache::with_policy(url, ttl, cooldown, negative_ttl, 1024)
    }

    #[tokio::test]
    async fn known_kid_served_from_cache_within_ttl_without_refetch() {
        // Pins: a `kid` hit inside the TTL is served from cache with no refetch.
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/jwks.json");
                then.status(200).json_body(jwks_body(&["kid-1"]));
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
    }

    #[tokio::test]
    async fn unknown_kid_probe_during_outage_does_not_evict_known_keys() {
        // Pins: F12 — a miss that triggers a failing refresh must not discard the
        // last-known-good set; known keys keep validating during a provider outage.
        let server = MockServer::start_async().await;
        let ok = server
            .mock_async(|when, then| {
                when.method(GET).path("/jwks.json");
                then.status(200).json_body(jwks_body(&["kid-1"]));
            })
            .await;
        let cache = cache_with(
            server.url("/jwks.json"),
            Duration::from_secs(3600),
            Duration::from_millis(1),
            Duration::from_secs(60),
        );

        cache.key_for("kid-1").await.expect("known kid resolves");
        assert_eq!(ok.hits_async().await, 1);

        // The endpoint now errors for every fetch.
        ok.delete_async().await;
        let outage = server
            .mock_async(|when, then| {
                when.method(GET).path("/jwks.json");
                then.status(500);
            })
            .await;
        tokio::time::sleep(Duration::from_millis(5)).await; // exit the refresh cooldown

        // An unknown kid drives a refresh that fails; it surfaces as a provider
        // error (not eviction), because the fetched set never arrived.
        match cache.key_for("kid-2").await {
            Err(JwksError::Http(_)) => {}
            Err(other) => panic!("expected a provider error, got {other:?}"),
            Ok(_) => panic!("unknown kid must not resolve against the retained set"),
        }
        assert!(
            outage.hits_async().await >= 1,
            "unknown kid attempted a refresh"
        );

        // The known kid still validates from the retained last-known-good set.
        cache
            .key_for("kid-1")
            .await
            .expect("known kid still served despite the outage");
    }

    #[tokio::test]
    async fn repeated_unknown_kid_probes_within_negative_ttl_fetch_at_most_once() {
        // Pins: F12 — an unknown kid confirmed absent by a successful refresh is
        // negative-cached, so repeated probes do not refetch even once the
        // cooldown has elapsed between them.
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/jwks.json");
                then.status(200).json_body(jwks_body(&["kid-1"]));
            })
            .await;
        let cache = cache_with(
            server.url("/jwks.json"),
            Duration::from_secs(3600),
            Duration::from_millis(1),
            Duration::from_secs(60),
        );

        cache.key_for("kid-1").await.expect("prime the cache");
        assert_eq!(mock.hits_async().await, 1);
        tokio::time::sleep(Duration::from_millis(5)).await; // exit cooldown

        // First probe of the unknown kid refreshes once and confirms it absent.
        assert!(matches!(
            cache.key_for("kid-x").await,
            Err(JwksError::UnknownKid(_))
        ));
        assert_eq!(
            mock.hits_async().await,
            2,
            "first unknown probe refreshes once"
        );

        // Repeated probes, each after the cooldown has elapsed, are served from
        // the negative cache without any further fetch.
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_millis(3)).await;
            assert!(matches!(
                cache.key_for("kid-x").await,
                Err(JwksError::UnknownKid(_))
            ));
        }
        assert_eq!(
            mock.hits_async().await,
            2,
            "repeated unknown probes are served from the negative cache"
        );
    }

    #[tokio::test]
    async fn stale_key_set_serves_last_known_good_when_refresh_fails() {
        // Pins: F12 — once the set is past its TTL and the provider is down, a
        // known kid is still served from the retained last-known-good set rather
        // than failing authentication.
        let server = MockServer::start_async().await;
        let ok = server
            .mock_async(|when, then| {
                when.method(GET).path("/jwks.json");
                then.status(200).json_body(jwks_body(&["kid-1"]));
            })
            .await;
        let cache = cache_with(
            server.url("/jwks.json"),
            Duration::from_millis(20),
            Duration::from_millis(1),
            Duration::from_secs(60),
        );

        cache.key_for("kid-1").await.expect("known kid resolves");

        ok.delete_async().await;
        let _outage = server
            .mock_async(|when, then| {
                when.method(GET).path("/jwks.json");
                then.status(500);
            })
            .await;
        // Exceed both the freshness TTL and the cooldown so a refresh is attempted.
        tokio::time::sleep(Duration::from_millis(30)).await;

        cache
            .key_for("kid-1")
            .await
            .expect("stale known kid served from last-known-good during the outage");
    }

    #[tokio::test]
    async fn new_key_is_rate_limited_within_cooldown_then_picked_up_after() {
        // Pins: F12 — a newly published kid is rejected without a fetch while the
        // cooldown is active (rate limit), then picked up by the first refresh
        // after the cooldown elapses.
        let server = MockServer::start_async().await;
        // Precompute both bodies so no slow RSA work runs inside the cooldown window.
        let body_one = jwks_body(&["kid-1"]);
        let body_both = jwks_body(&["kid-1", "kid-2"]);
        let first = server
            .mock_async(|when, then| {
                when.method(GET).path("/jwks.json");
                then.status(200).json_body(body_one);
            })
            .await;
        let cache = cache_with(
            server.url("/jwks.json"),
            Duration::from_secs(3600),
            Duration::from_millis(300),
            Duration::from_secs(60),
        );

        cache.key_for("kid-1").await.expect("known kid resolves");
        assert_eq!(first.hits_async().await, 1);

        // Publish the new key.
        first.delete_async().await;
        let second = server
            .mock_async(|when, then| {
                when.method(GET).path("/jwks.json");
                then.status(200).json_body(body_both);
            })
            .await;

        // Within the cooldown, the new kid is rejected as an invalid credential
        // with no refetch.
        assert!(matches!(
            cache.key_for("kid-2").await,
            Err(JwksError::UnknownKid(_))
        ));
        assert_eq!(
            second.hits_async().await,
            0,
            "no refetch within the cooldown window"
        );

        // After the cooldown elapses, the next probe refreshes and picks it up.
        tokio::time::sleep(Duration::from_millis(350)).await;
        cache
            .key_for("kid-2")
            .await
            .expect("new key picked up after the cooldown");
        assert_eq!(second.hits_async().await, 1);
    }
}

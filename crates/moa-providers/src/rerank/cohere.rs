//! Cohere rerank provider client.

use async_trait::async_trait;
use moa_core::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::core::concurrency::{ConcurrencyLimiter, DEFAULT_MAX_IN_FLIGHT};
use crate::core::http::{build_json_http_client, post_json};
use crate::core::pacer::{PacerConfig, RatePacer};
use crate::core::rate_guard;

use super::{RerankHit, Reranker};

const COHERE_RERANK_URL: &str = "https://api.cohere.com/v2/rerank";
/// Default Cohere Rerank v4 model used for low-latency memory retrieval.
pub const COHERE_DEFAULT_RERANK_MODEL: &str = "rerank-v4.0-fast";
/// Documented Cohere Rerank production limit: 1,000 requests/min.
const COHERE_RERANK_REQUESTS_PER_MIN: u32 = 1_000;

/// Cohere Rerank v4 client.
#[derive(Clone)]
pub struct CohereReranker {
    client: Client,
    api_key: String,
    endpoint: String,
    pacer: RatePacer,
    limiter: ConcurrencyLimiter,
}

impl CohereReranker {
    /// Creates a reranker using Cohere's production endpoint.
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        Ok(Self {
            client: build_json_http_client()?,
            api_key: api_key.into(),
            endpoint: COHERE_RERANK_URL.to_string(),
            pacer: RatePacer::new(PacerConfig::requests_per_min(
                COHERE_RERANK_REQUESTS_PER_MIN,
            )),
            limiter: ConcurrencyLimiter::new(DEFAULT_MAX_IN_FLIGHT),
        })
    }

    /// Overrides the Cohere endpoint, primarily for tests.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Overrides the request/input pacing, e.g. to apply trial-tier limits.
    #[must_use]
    pub fn with_rate_limits(mut self, config: PacerConfig) -> Self {
        self.pacer = RatePacer::new(config);
        self
    }

    /// Overrides the in-flight concurrency ceiling for rerank requests.
    #[must_use]
    /// Replaces the in-flight concurrency limiter (config-driven or global).
    pub(crate) fn with_limiter(mut self, limiter: ConcurrencyLimiter) -> Self {
        self.limiter = limiter;
        self
    }

    pub fn with_max_concurrent_requests(mut self, max_in_flight: usize) -> Self {
        self.limiter = ConcurrencyLimiter::new(max_in_flight);
        self
    }
}

#[async_trait]
impl Reranker for CohereReranker {
    async fn rerank(
        &self,
        model: &str,
        query: &str,
        documents: &[String],
        top_n: usize,
    ) -> Result<Vec<RerankHit>> {
        if documents.is_empty() || top_n == 0 {
            return Ok(Vec::new());
        }

        // In-flight slot first, then rate budget (see `ConcurrencyLimiter`).
        let _permit = match self.limiter.acquire().await {
            Some(lease) => lease,
            None => {
                return Err(rate_guard::rate_limited_saturated(
                    self.limiter.block_threshold(),
                ));
            }
        };
        // Cohere Rerank is limited by requests/min.
        self.pacer.acquire(1, 0).await;
        let body: CohereRerankResponse = post_json(
            &self.client,
            &self.endpoint,
            &self.api_key,
            &CohereRerankRequest {
                model,
                query,
                documents,
                top_n,
            },
        )
        .await?;
        Ok(body
            .results
            .into_iter()
            .filter(|hit| hit.index < documents.len())
            .map(|hit| RerankHit {
                index: hit.index,
                relevance_score: hit.relevance_score,
            })
            .collect())
    }
}

#[derive(Serialize)]
struct CohereRerankRequest<'a> {
    model: &'a str,
    query: &'a str,
    documents: &'a [String],
    top_n: usize,
}

#[derive(Deserialize)]
struct CohereRerankResponse {
    results: Vec<CohereRerankResponseHit>,
}

#[derive(Deserialize)]
struct CohereRerankResponseHit {
    index: usize,
    relevance_score: f32,
}

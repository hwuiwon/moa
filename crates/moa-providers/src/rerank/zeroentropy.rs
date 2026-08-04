//! ZeroEntropy rerank provider client.

use async_trait::async_trait;
use moa_core::{error::MoaError, error::Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::Instrument;

use crate::core::concurrency::{ConcurrencyLimiter, DEFAULT_MAX_IN_FLIGHT};
use crate::core::concurrency_factory::ProviderCoordination;
use crate::core::http::{build_json_http_client, post_json};
use crate::core::instrumentation::{
    fail_provider_span, finish_rerank_span, provider_error_class, rerank_span,
};
use crate::core::pacer::{PacerConfig, RatePacer};
use crate::core::rate_guard;

use super::{RerankHit, Reranker};

const ZEROENTROPY_RERANK_URL: &str = "https://api.zeroentropy.dev/v1/models/rerank";
const ZEROENTROPY_RERANK_PROVIDER: &str = "zeroentropy";
/// Default ZeroEntropy rerank model.
pub const ZEROENTROPY_DEFAULT_RERANK_MODEL: &str = "zerank-2";

/// ZeroEntropy latency mode for rerank calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZeroEntropyRerankLatency {
    /// Force fast inference and fail with 429 when fast capacity is exhausted.
    Fast,
    /// Allow the slower high-throughput inference lane.
    Slow,
}

impl ZeroEntropyRerankLatency {
    pub(super) fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "fast" => Ok(Self::Fast),
            "slow" => Ok(Self::Slow),
            other => Err(MoaError::ConfigError(format!(
                "zeroentropy reranker latency must be fast or slow, got `{other}`"
            ))),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Slow => "slow",
        }
    }
}

/// ZeroEntropy rerank client backed by the `/v1/models/rerank` endpoint.
#[derive(Clone)]
pub struct ZeroEntropyReranker {
    client: Client,
    api_key: String,
    endpoint: String,
    latency: Option<ZeroEntropyRerankLatency>,
    pacer: RatePacer,
    limiter: ConcurrencyLimiter,
}

impl ZeroEntropyReranker {
    /// Creates a ZeroEntropy reranker using the production endpoint.
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        Ok(Self {
            client: build_json_http_client()?,
            api_key: api_key.into(),
            endpoint: ZEROENTROPY_RERANK_URL.to_string(),
            latency: None,
            // Pacing off by default; ZeroEntropy limits are tier-specific.
            pacer: RatePacer::new(PacerConfig::disabled()),
            limiter: ConcurrencyLimiter::new(DEFAULT_MAX_IN_FLIGHT),
        })
    }

    /// Overrides the ZeroEntropy endpoint, primarily for tests.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Enables request/input pacing, off by default for tier-specific limits.
    #[must_use]
    pub fn with_rate_limits(mut self, config: PacerConfig) -> Self {
        self.pacer = RatePacer::new(config);
        self
    }
    /// Attaches the fleet-shared per-minute quota for this credential.
    ///
    /// Applied after any pacing override so the effective limits — the
    /// provider's built-in defaults or the operator's — are the ones coordinated.
    #[must_use]
    pub(crate) fn with_shared_pacing(
        mut self,
        coordination: &ProviderCoordination,
        provider: &str,
        credential: &str,
    ) -> Self {
        self.pacer = coordination.share_pacing(self.pacer, provider, credential);
        self
    }

    /// Overrides the in-flight concurrency ceiling for rerank requests.
    #[must_use]
    /// Replaces the in-flight concurrency limiter (config-driven or global).
    pub(crate) fn with_limiter(mut self, limiter: ConcurrencyLimiter) -> Self {
        self.limiter = limiter;
        self
    }

    /// Sets the optional ZeroEntropy latency mode.
    #[must_use]
    pub fn with_latency(mut self, latency: Option<ZeroEntropyRerankLatency>) -> Self {
        self.latency = latency;
        self
    }
}

#[async_trait]
impl Reranker for ZeroEntropyReranker {
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
        self.pacer.acquire(model, 1, 0).await?;
        let span = rerank_span(ZEROENTROPY_RERANK_PROVIDER, model, documents.len());
        let result: Result<ZeroEntropyRerankResponse> = post_json(
            &self.client,
            &self.endpoint,
            &self.api_key,
            &ZeroEntropyRerankRequest {
                model,
                query,
                documents,
                top_n: Some(top_n),
                latency: self.latency.map(ZeroEntropyRerankLatency::as_str),
            },
        )
        .instrument(span.clone())
        .await;
        let body = match result {
            Ok(body) => body,
            Err(error) => {
                fail_provider_span(&span, provider_error_class(&error), &error);
                return Err(error);
            }
        };
        let total_tokens = body.total_tokens;
        finish_rerank_span(&span, model, Some(total_tokens));
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
struct ZeroEntropyRerankRequest<'a> {
    model: &'a str,
    query: &'a str,
    documents: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    top_n: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    latency: Option<&'static str>,
}

#[derive(Deserialize)]
struct ZeroEntropyRerankResponse {
    results: Vec<ZeroEntropyRerankResponseHit>,
    total_tokens: usize,
}

#[derive(Deserialize)]
struct ZeroEntropyRerankResponseHit {
    index: usize,
    relevance_score: f32,
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::core::span_capture_test_support::{
        attr_f64, attr_i64, capture_spans_async, find_span,
    };

    use super::{Reranker, ZEROENTROPY_DEFAULT_RERANK_MODEL, ZeroEntropyReranker};

    #[tokio::test]
    async fn rerank_records_provider_token_usage_and_exact_cost() {
        // Pins: the real adapter records ZeroEntropy's response-reported usage
        // and prices zerank-2 at $0.025/1M tokens without estimating usage.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{ "index": 0, "relevance_score": 0.9 }],
                "total_tokens": 400
            })))
            .mount(&server)
            .await;
        let reranker = ZeroEntropyReranker::new("test-key")
            .expect("ZeroEntropy reranker should build")
            .with_endpoint(server.uri());
        let documents = vec!["one".to_string(), "two".to_string()];

        let spans = capture_spans_async(async {
            reranker
                .rerank(ZEROENTROPY_DEFAULT_RERANK_MODEL, "query", &documents, 1)
                .await
                .expect("wiremock rerank request should succeed");
        })
        .await;

        let span = find_span(&spans, "rerank zerank-2");
        assert_eq!(attr_i64(span, "gen_ai.usage.input_tokens"), Some(400));
        let cost = attr_f64(span, "moa.rerank.cost_usd").expect("cost should be recorded");
        assert!((cost - 0.00001).abs() < 1e-12);
    }
}

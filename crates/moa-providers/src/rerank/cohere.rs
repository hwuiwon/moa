//! Cohere rerank provider client.

use async_trait::async_trait;
use moa_core::error::Result;
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

const COHERE_RERANK_URL: &str = "https://api.cohere.com/v2/rerank";
const COHERE_RERANK_PROVIDER: &str = "cohere";
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
        self.pacer.acquire(model, 1, 0).await?;
        let span = rerank_span(COHERE_RERANK_PROVIDER, model, documents.len());
        let result: Result<CohereRerankResponse> = post_json(
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
        .instrument(span.clone())
        .await;
        let body = match result {
            Ok(body) => body,
            Err(error) => {
                fail_provider_span(&span, provider_error_class(&error), &error);
                return Err(error);
            }
        };
        finish_rerank_span(&span, model, None);
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

#[cfg(test)]
mod tests {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::core::span_capture_test_support::{
        attr_f64, attr_i64, attr_string, capture_spans_async, find_span,
    };

    use super::{CohereReranker, Reranker};

    #[tokio::test]
    async fn rerank_records_flat_per_call_cost_from_pricing_catalog() {
        // Pins: a successful rerank call records document_count and a flat
        // per-call cost from the dedicated RERANK_CATALOG price
        // (rerank-v4.0-fast is $2.00/1K searches -> $0.002/call), regardless
        // of how many documents were reranked.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{ "index": 0, "relevance_score": 0.9 }]
            })))
            .mount(&server)
            .await;
        let reranker = CohereReranker::new("test-key")
            .expect("Cohere reranker should build")
            .with_endpoint(server.uri());
        let documents = vec!["a".to_string(), "b".to_string()];

        let spans = capture_spans_async(async {
            reranker
                .rerank("rerank-v4.0-fast", "query", &documents, 1)
                .await
                .expect("wiremock rerank request should succeed");
        })
        .await;

        let span = find_span(&spans, "rerank rerank-v4.0-fast");
        assert_eq!(
            attr_string(span, "gen_ai.provider.name").as_deref(),
            Some("cohere")
        );
        assert_eq!(attr_i64(span, "moa.rerank.document_count"), Some(2));
        let cost = attr_f64(span, "moa.rerank.cost_usd").expect("cost should be recorded");
        assert!((cost - 0.002).abs() < 1e-12);
    }

    #[tokio::test]
    async fn rerank_marks_span_failed_with_bounded_error_type_on_http_error() {
        // Pins: an upstream server error marks the rerank span with an OTel
        // error status and the bounded 5xx error class, not a raw message.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503).set_body_string("try again"))
            .mount(&server)
            .await;
        let reranker = CohereReranker::new("test-key")
            .expect("Cohere reranker should build")
            .with_endpoint(server.uri());
        let documents = vec!["a".to_string()];

        let spans = capture_spans_async(async {
            let result = reranker
                .rerank("rerank-v4.0-fast", "query", &documents, 1)
                .await;
            assert!(result.is_err(), "a 503 response should surface as an error");
        })
        .await;

        let span = find_span(&spans, "rerank rerank-v4.0-fast");
        assert_eq!(attr_string(span, "error.type").as_deref(), Some("http_5xx"));
        assert!(
            matches!(span.status, opentelemetry::trace::Status::Error { .. }),
            "expected an OTel error status, got {:?}",
            span.status
        );
    }
}

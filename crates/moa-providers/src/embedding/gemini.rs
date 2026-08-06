//! Gemini embedding provider client.
//!
//! `gemini-embedding-2` is exposed through MOA's existing text-only
//! [`EmbeddingProvider`](moa_core::traits::EmbeddingProvider) trait. The API is
//! multimodal, but binary chunking and sandboxed media handling are out of
//! scope for this provider adapter.

use async_trait::async_trait;
use moa_core::traits::EmbeddingProvider;
use moa_core::{error::MoaError, error::Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::Instrument;

use crate::core::concurrency::{ConcurrencyLimiter, DEFAULT_MAX_IN_FLIGHT};
use crate::core::concurrency_factory::ProviderCoordination;
use crate::core::http::{
    build_json_http_client, decode_json_response, validate_embedding_count,
    validate_embedding_dimension,
};
use crate::core::instrumentation::{
    embedding_span, fail_provider_span, finish_embedding_span, provider_error_class,
};
use crate::core::pacer::{PacerConfig, RatePacer};
use crate::core::rate_guard;

const GEMINI_EMBEDDING_PROVIDER: &str = "google";

const GEMINI_ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta";
pub(super) const GEMINI_V2_MODEL: &str = "gemini-embedding-2";
/// Maximum inputs Gemini's `:batchEmbedContents` endpoint accepts per request.
const GEMINI_MAX_BATCH_SIZE: usize = 100;

/// Construction role used to pin asymmetric retrieval prefixes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbedderConstructionRole {
    /// Build an ingestion-side document embedder.
    Ingestion,
    /// Build a retrieval-side query embedder.
    Retrieval,
}

impl EmbedderConstructionRole {
    fn format(self, content: &str) -> String {
        match self {
            Self::Ingestion => format!("title: none | text: {content}"),
            Self::Retrieval => format!("task: search result | query: {content}"),
        }
    }
}

/// Gemini text embedder backed by `gemini-embedding-2`.
#[derive(Clone)]
pub struct GeminiEmbeddingEmbedder {
    client: Client,
    api_key: String,
    endpoint: String,
    output_dim: usize,
    role: EmbedderConstructionRole,
    pacer: RatePacer,
    limiter: ConcurrencyLimiter,
}

impl GeminiEmbeddingEmbedder {
    /// Creates a Gemini embedder.
    pub fn new(
        api_key: impl Into<String>,
        output_dim: usize,
        role: EmbedderConstructionRole,
    ) -> Result<Self> {
        validate_gemini_output_dim(output_dim)?;
        Ok(Self {
            client: build_json_http_client()?,
            api_key: api_key.into(),
            endpoint: GEMINI_ENDPOINT.to_string(),
            output_dim,
            role,
            // Pacing off by default; Gemini limits are tier-specific.
            pacer: RatePacer::new(PacerConfig::disabled()),
            limiter: ConcurrencyLimiter::new(DEFAULT_MAX_IN_FLIGHT),
        })
    }

    /// Overrides the endpoint base URL, primarily for tests.
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

    /// Overrides the in-flight concurrency ceiling for embedding requests.
    #[must_use]
    /// Replaces the in-flight concurrency limiter (config-driven or global).
    pub(crate) fn with_limiter(mut self, limiter: ConcurrencyLimiter) -> Self {
        self.limiter = limiter;
        self
    }

    /// Embeds one chunk of inputs in a single `:batchEmbedContents` round trip.
    ///
    /// Callers must keep `texts` within [`GEMINI_MAX_BATCH_SIZE`]; the returned
    /// embeddings preserve request order one-to-one with `texts`.
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        // In-flight slot first, then rate budget (see `ConcurrencyLimiter`).
        let _permit = match self.limiter.acquire().await {
            Some(lease) => lease,
            None => {
                return Err(rate_guard::rate_limited_saturated(
                    self.limiter.block_threshold(),
                ));
            }
        };
        self.pacer
            .acquire(GEMINI_V2_MODEL, 1, texts.len() as u32)
            .await?;
        let requests = texts
            .iter()
            .map(|text| BatchEmbedItem {
                model: format!("models/{GEMINI_V2_MODEL}"),
                content: GeminiContent {
                    parts: vec![GeminiTextPart {
                        text: self.role.format(text),
                    }],
                },
                output_dimensionality: Some(self.output_dim),
            })
            .collect();
        let body = BatchEmbedRequest { requests };
        let span = embedding_span(GEMINI_EMBEDDING_PROVIDER, GEMINI_V2_MODEL, texts.len());
        let result = self
            .post_batch_embed(GEMINI_V2_MODEL, &body)
            .instrument(span.clone())
            .await;
        let response = match result {
            Ok(response) => response,
            Err(error) => {
                fail_provider_span(&span, provider_error_class(&error), &error);
                return Err(error);
            }
        };
        if let Some(input_tokens) = response
            .usage_metadata
            .as_ref()
            .and_then(|usage| usage.prompt_token_count)
        {
            finish_embedding_span(&span, GEMINI_V2_MODEL, input_tokens);
        }
        validate_embedding_count(texts.len(), response.embeddings.len())?;
        let mut out = Vec::with_capacity(texts.len());
        for embedding in response.embeddings {
            validate_embedding_dimension(self.output_dim, &embedding.values)?;
            out.push(embedding.values);
        }
        Ok(out)
    }

    async fn post_batch_embed<T: Serialize>(
        &self,
        model: &str,
        body: &T,
    ) -> Result<BatchEmbedResponse> {
        self.post_json(&format!("models/{model}:batchEmbedContents"), body)
            .await
    }

    async fn post_json<Req, Resp>(&self, path: &str, body: &Req) -> Result<Resp>
    where
        Req: Serialize + ?Sized,
        Resp: serde::de::DeserializeOwned,
    {
        let response = self
            .client
            .post(format!("{}/{path}", self.endpoint.trim_end_matches('/')))
            .header("x-goog-api-key", &self.api_key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(body)
            .send()
            .await
            .map_err(crate::core::http::provider_transport_error)?;
        decode_json_response(response).await
    }
}

#[async_trait]
impl EmbeddingProvider for GeminiEmbeddingEmbedder {
    fn model_id(&self) -> &str {
        GEMINI_V2_MODEL
    }

    fn model_version(&self) -> i32 {
        2
    }

    fn dimensions(&self) -> usize {
        self.output_dim
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(GEMINI_MAX_BATCH_SIZE) {
            out.extend(self.embed_batch(chunk).await?);
        }
        Ok(out)
    }
}

#[derive(Serialize)]
struct BatchEmbedRequest {
    requests: Vec<BatchEmbedItem>,
}

#[derive(Serialize)]
struct BatchEmbedItem {
    model: String,
    content: GeminiContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_dimensionality: Option<usize>,
}

#[derive(Deserialize)]
struct BatchEmbedResponse {
    embeddings: Vec<GeminiEmbedding>,
    #[serde(default, rename = "usageMetadata")]
    usage_metadata: Option<GeminiEmbeddingUsageMetadata>,
}

#[derive(Deserialize)]
struct GeminiEmbeddingUsageMetadata {
    #[serde(default, rename = "promptTokenCount")]
    prompt_token_count: Option<usize>,
}

#[derive(Serialize)]
struct GeminiContent {
    parts: Vec<GeminiTextPart>,
}

#[derive(Serialize)]
struct GeminiTextPart {
    text: String,
}

#[derive(Deserialize)]
struct GeminiEmbedding {
    values: Vec<f32>,
}

fn validate_gemini_output_dim(output_dim: usize) -> Result<()> {
    if (128..=3072).contains(&output_dim) {
        Ok(())
    } else {
        Err(MoaError::ConfigError(format!(
            "Gemini output_dim must be in 128..=3072, got {output_dim}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::EmbedderConstructionRole;

    #[test]
    fn role_prefixes_match_documented_shapes() {
        assert!(
            EmbedderConstructionRole::Retrieval
                .format("oauth")
                .starts_with("task: search result | query: ")
        );
        assert!(
            EmbedderConstructionRole::Ingestion
                .format("oauth")
                .starts_with("title: none | text: ")
        );
    }
}

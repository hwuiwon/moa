//! Gemini embedding provider client.
//!
//! `gemini-embedding-2` is exposed through MOA's existing text-only
//! [`EmbeddingProvider`](moa_core::traits::EmbeddingProvider) trait. The API is
//! multimodal, but binary chunking and sandboxed media handling are out of
//! scope for this provider adapter.

use async_trait::async_trait;
use moa_core::traits::EmbeddingProvider;
use moa_core::{MoaError, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::core::concurrency::{
    ConcurrencyLimiter, DEFAULT_BLOCK_THRESHOLD, DEFAULT_EMBEDDING_CONCURRENCY,
};
use crate::core::http::{
    build_json_http_client, decode_json_response, validate_embedding_count,
    validate_embedding_dimension,
};
use crate::core::pacer::{PacerConfig, RatePacer};
use crate::core::rate_guard;

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
            limiter: ConcurrencyLimiter::new(DEFAULT_EMBEDDING_CONCURRENCY),
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

    /// Overrides the in-flight concurrency ceiling for embedding requests.
    #[must_use]
    pub fn with_max_concurrent_requests(mut self, max_in_flight: usize) -> Self {
        self.limiter = ConcurrencyLimiter::new(max_in_flight);
        self
    }

    /// Embeds one chunk of inputs in a single `:batchEmbedContents` round trip.
    ///
    /// Callers must keep `texts` within [`GEMINI_MAX_BATCH_SIZE`]; the returned
    /// embeddings preserve request order one-to-one with `texts`.
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        // In-flight slot first, then rate budget (see `ConcurrencyLimiter`).
        let _permit = match self.limiter.acquire_within(DEFAULT_BLOCK_THRESHOLD).await {
            Some(lease) => lease,
            None => return Err(rate_guard::rate_limited_saturated(DEFAULT_BLOCK_THRESHOLD)),
        };
        self.pacer.acquire(1, texts.len() as u32).await;
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
        let response = self.post_batch_embed(GEMINI_V2_MODEL, &body).await?;
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
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
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

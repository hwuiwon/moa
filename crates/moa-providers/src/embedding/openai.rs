//! OpenAI embedding provider client.

use async_trait::async_trait;
use moa_core::Result;
use moa_core::traits::EmbeddingProvider;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::core::concurrency::{ConcurrencyLimiter, DEFAULT_EMBEDDING_CONCURRENCY};
use crate::core::http::{
    build_json_http_client, post_json, validate_embedding_count, validate_embedding_dimension,
};
use crate::core::pacer::{PacerConfig, RatePacer};

const OPENAI_EMBEDDINGS_URL: &str = "https://api.openai.com/v1/embeddings";
const OPENAI_DIMENSIONS: usize = 1_536;
/// Maximum inputs the OpenAI `/v1/embeddings` endpoint accepts per request.
const OPENAI_MAX_INPUTS_PER_REQUEST: usize = 2_048;

/// OpenAI embeddings client backed by the `/v1/embeddings` endpoint.
#[derive(Clone)]
pub struct OpenAIEmbedding {
    client: Client,
    api_key: String,
    model: String,
    embeddings_url: String,
    pacer: RatePacer,
    limiter: ConcurrencyLimiter,
}

impl OpenAIEmbedding {
    /// Creates an OpenAI embedding client from an API key and model id.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Result<Self> {
        Ok(Self {
            client: build_json_http_client()?,
            api_key: api_key.into(),
            model: model.into(),
            embeddings_url: OPENAI_EMBEDDINGS_URL.to_string(),
            // Pacing off by default; OpenAI limits are tier-specific.
            pacer: RatePacer::new(PacerConfig::disabled()),
            limiter: ConcurrencyLimiter::new(DEFAULT_EMBEDDING_CONCURRENCY),
        })
    }

    /// Overrides the embeddings URL, primarily for HTTP-level tests.
    #[must_use]
    pub fn with_embeddings_url(mut self, embeddings_url: impl Into<String>) -> Self {
        self.embeddings_url = embeddings_url.into();
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

    /// Embeds one chunk of inputs in a single `/v1/embeddings` round trip.
    ///
    /// Callers must keep `inputs` within [`OPENAI_MAX_INPUTS_PER_REQUEST`]; the
    /// returned embeddings preserve request order one-to-one with `inputs`.
    async fn embed_chunk(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        // In-flight slot first, then rate budget (see `ConcurrencyLimiter`).
        let _permit = self.limiter.acquire().await;
        self.pacer.acquire(1, inputs.len() as u32).await;
        let payload: OpenAIEmbeddingResponse = post_json(
            &self.client,
            &self.embeddings_url,
            &self.api_key,
            &OpenAIEmbeddingRequest {
                model: self.model.clone(),
                input: inputs.to_vec(),
                encoding_format: "float".to_string(),
            },
        )
        .await?;
        validate_embedding_count(inputs.len(), payload.data.len())?;

        let mut data = payload.data;
        data.sort_by_key(|item| item.index);
        let embeddings: Vec<Vec<f32>> = data.into_iter().map(|item| item.embedding).collect();
        // Reject vectors whose width does not match the model's fixed
        // dimensionality, mirroring the Cohere/Gemini/ZeroEntropy embedders so a
        // truncated or malformed response cannot silently poison the vector store.
        for embedding in &embeddings {
            validate_embedding_dimension(OPENAI_DIMENSIONS, embedding)?;
        }
        Ok(embeddings)
    }
}

#[async_trait]
impl EmbeddingProvider for OpenAIEmbedding {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn dimensions(&self) -> usize {
        OPENAI_DIMENSIONS
    }

    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }

        let mut embeddings = Vec::with_capacity(inputs.len());
        for chunk in inputs.chunks(OPENAI_MAX_INPUTS_PER_REQUEST) {
            embeddings.extend(self.embed_chunk(chunk).await?);
        }
        Ok(embeddings)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct OpenAIEmbeddingRequest {
    model: String,
    input: Vec<String>,
    encoding_format: String,
}

#[derive(Debug, Clone, Deserialize)]
struct OpenAIEmbeddingResponse {
    data: Vec<OpenAIEmbeddingData>,
}

#[derive(Debug, Clone, Deserialize)]
struct OpenAIEmbeddingData {
    index: usize,
    embedding: Vec<f32>,
}

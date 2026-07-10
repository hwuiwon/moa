//! ZeroEntropy embedding provider client.

use async_trait::async_trait;
use moa_core::traits::EmbeddingProvider;
use moa_core::{MoaError, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::core::concurrency::{
    ConcurrencyLimiter, DEFAULT_BLOCK_THRESHOLD, DEFAULT_EMBEDDING_CONCURRENCY,
};
use crate::core::http::{
    build_json_http_client, post_json, validate_embedding_count, validate_embedding_dimension,
};
use crate::core::pacer::{PacerConfig, RatePacer};
use crate::core::rate_guard;

const ZEROENTROPY_EMBEDDINGS_URL: &str = "https://api.zeroentropy.dev/v1/models/embed";
/// Default ZeroEntropy embedding model id, used as a fixture by selector tests.
#[cfg(test)]
pub(super) const ZEROENTROPY_DEFAULT_MODEL: &str = "zembed-1";
const ZEROENTROPY_DEFAULT_INPUT_TYPE: &str = "document";
const ZEROENTROPY_DEFAULT_DIMENSIONS: usize = 1_280;
const ZEROENTROPY_MAX_TEXTS: usize = 100;
const ZEROENTROPY_SUPPORTED_DIMENSIONS: [usize; 7] = [2_560, 1_280, 640, 320, 160, 80, 40];

/// ZeroEntropy embedding client backed by the `/v1/models/embed` endpoint.
#[derive(Clone)]
pub struct ZeroEntropyEmbedding {
    client: Client,
    api_key: String,
    model: String,
    embeddings_url: String,
    input_type: String,
    dimensions: usize,
    pacer: RatePacer,
    limiter: ConcurrencyLimiter,
}

impl ZeroEntropyEmbedding {
    /// Creates a ZeroEntropy embedding client from an API key and model id.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Result<Self> {
        Ok(Self {
            client: build_json_http_client()?,
            api_key: api_key.into(),
            model: model.into(),
            embeddings_url: ZEROENTROPY_EMBEDDINGS_URL.to_string(),
            input_type: ZEROENTROPY_DEFAULT_INPUT_TYPE.to_string(),
            dimensions: ZEROENTROPY_DEFAULT_DIMENSIONS,
            // Pacing off by default; ZeroEntropy limits are tier-specific.
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

    /// Overrides the fixed output dimensionality expected from ZeroEntropy.
    pub fn with_dimensions(mut self, dimensions: usize) -> Result<Self> {
        if !ZEROENTROPY_SUPPORTED_DIMENSIONS.contains(&dimensions) {
            return Err(MoaError::ConfigError(format!(
                "zeroentropy embedding output dimensions must be one of {:?}, got {dimensions}",
                ZEROENTROPY_SUPPORTED_DIMENSIONS
            )));
        }
        self.dimensions = dimensions;
        Ok(self)
    }

    async fn embed_chunk(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        // In-flight slot first, then rate budget (see `ConcurrencyLimiter`).
        let _permit = match self.limiter.acquire_within(DEFAULT_BLOCK_THRESHOLD).await {
            Some(lease) => lease,
            None => return Err(rate_guard::rate_limited_saturated(DEFAULT_BLOCK_THRESHOLD)),
        };
        self.pacer.acquire(1, inputs.len() as u32).await;
        let payload: ZeroEntropyEmbeddingResponse = post_json(
            &self.client,
            &self.embeddings_url,
            &self.api_key,
            &ZeroEntropyEmbeddingRequest {
                model: self.model.clone(),
                input_type: self.input_type.clone(),
                input: inputs.to_vec(),
                dimensions: self.dimensions,
                encoding_format: ZEROENTROPY_FLOAT_ENCODING.to_string(),
            },
        )
        .await?;
        validate_embedding_count(inputs.len(), payload.results.len())?;

        let embeddings: Vec<Vec<f32>> = payload
            .results
            .into_iter()
            .map(|result| result.embedding)
            .collect();
        for embedding in &embeddings {
            validate_embedding_dimension(self.dimensions, embedding)?;
        }
        Ok(embeddings)
    }
}

#[async_trait]
impl EmbeddingProvider for ZeroEntropyEmbedding {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }

        let mut embeddings = Vec::with_capacity(inputs.len());
        for chunk in inputs.chunks(ZEROENTROPY_MAX_TEXTS) {
            embeddings.extend(self.embed_chunk(chunk).await?);
        }
        Ok(embeddings)
    }
}

const ZEROENTROPY_FLOAT_ENCODING: &str = "float";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ZeroEntropyEmbeddingRequest {
    model: String,
    input_type: String,
    input: Vec<String>,
    dimensions: usize,
    encoding_format: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ZeroEntropyEmbeddingResponse {
    results: Vec<ZeroEntropyEmbeddingResult>,
}

#[derive(Debug, Clone, Deserialize)]
struct ZeroEntropyEmbeddingResult {
    embedding: Vec<f32>,
}

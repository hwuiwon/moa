//! Cohere embedding provider client.

use async_trait::async_trait;
use futures_util::future::try_join_all;
use moa_core::traits::EmbeddingProvider;
use moa_core::{error::MoaError, error::Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::core::concurrency::{ConcurrencyLimiter, DEFAULT_MAX_IN_FLIGHT};
use crate::core::http::{
    build_json_http_client, post_json, validate_embedding_count, validate_embedding_dimension,
};
use crate::core::pacer::{PacerConfig, RatePacer};
use crate::core::rate_guard;

const COHERE_EMBEDDINGS_URL: &str = "https://api.cohere.com/v2/embed";
pub(super) const COHERE_DEFAULT_MODEL: &str = "embed-v4.0";
const COHERE_DOCUMENT_INPUT_TYPE: &str = "search_document";
const COHERE_QUERY_INPUT_TYPE: &str = "search_query";
const COHERE_DEFAULT_INPUT_TYPE: &str = COHERE_DOCUMENT_INPUT_TYPE;
const COHERE_FLOAT_EMBEDDING_TYPE: &str = "float";
const COHERE_MAX_TEXTS: usize = 96;
/// Maximum embedding chunks Cohere requests are allowed to have in flight at once.
const COHERE_EMBED_CONCURRENCY: usize = 4;
/// Documented Cohere Embed (text) limit: 2,000 inputs/min (trial and production).
const COHERE_EMBED_INPUTS_PER_MIN: u32 = 2_000;
const COHERE_DEFAULT_DIMENSIONS: usize = 1_536;
const COHERE_GRAPH_MEMORY_DIMENSIONS: usize = 1_024;

/// Cohere Embed v4 client backed by the `/v2/embed` endpoint.
#[derive(Clone)]
pub struct CohereEmbedding {
    client: Client,
    api_key: String,
    model: String,
    embeddings_url: String,
    input_type: String,
    dimensions: usize,
    pacer: RatePacer,
    limiter: ConcurrencyLimiter,
}

impl CohereEmbedding {
    /// Creates a Cohere embedding client from an API key and model id.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Result<Self> {
        Ok(Self {
            client: build_json_http_client()?,
            api_key: api_key.into(),
            model: model.into(),
            embeddings_url: COHERE_EMBEDDINGS_URL.to_string(),
            input_type: COHERE_DEFAULT_INPUT_TYPE.to_string(),
            dimensions: COHERE_DEFAULT_DIMENSIONS,
            pacer: RatePacer::new(PacerConfig::inputs_per_min(COHERE_EMBED_INPUTS_PER_MIN)),
            limiter: ConcurrencyLimiter::new(DEFAULT_MAX_IN_FLIGHT),
        })
    }

    /// Overrides the embeddings URL, primarily for HTTP-level tests.
    #[must_use]
    pub fn with_embeddings_url(mut self, embeddings_url: impl Into<String>) -> Self {
        self.embeddings_url = embeddings_url.into();
        self
    }

    /// Overrides the Cohere retrieval role for this embedder.
    #[must_use]
    pub fn with_input_type(mut self, input_type: impl Into<String>) -> Self {
        self.input_type = input_type.into();
        self
    }

    /// Overrides the request/input pacing, e.g. to apply trial-tier limits.
    #[must_use]
    pub fn with_rate_limits(mut self, config: PacerConfig) -> Self {
        self.pacer = RatePacer::new(config);
        self
    }

    /// Overrides the in-flight concurrency ceiling for embedding requests.
    #[must_use]
    /// Replaces the in-flight concurrency limiter (config-driven or global).
    pub(crate) fn with_limiter(mut self, limiter: ConcurrencyLimiter) -> Self {
        self.limiter = limiter;
        self
    }

    /// Overrides the fixed output dimensionality expected from Cohere.
    pub fn with_dimensions(mut self, dimensions: usize) -> Result<Self> {
        if dimensions == 0 {
            return Err(MoaError::ConfigError(
                "cohere embedding output dimensions must be greater than zero".to_string(),
            ));
        }
        self.dimensions = dimensions;
        Ok(self)
    }

    async fn embed_chunk(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        // Take an in-flight slot before spending rate budget, then hold it across
        // the round trip (see `ConcurrencyLimiter` for the ordering rationale).
        let _permit = match self.limiter.acquire().await {
            Some(lease) => lease,
            None => {
                return Err(rate_guard::rate_limited_saturated(
                    self.limiter.block_threshold(),
                ));
            }
        };
        // Cohere Embed is limited by inputs/min; pace on this chunk's input count.
        self.pacer.acquire(1, inputs.len() as u32).await;
        let payload: CohereEmbeddingResponse = post_json(
            &self.client,
            &self.embeddings_url,
            &self.api_key,
            &CohereEmbeddingRequest {
                model: self.model.clone(),
                texts: inputs.to_vec(),
                input_type: self.input_type.clone(),
                embedding_types: vec![COHERE_FLOAT_EMBEDDING_TYPE.to_string()],
                output_dimension: self.dimensions,
            },
        )
        .await?;
        let embeddings = payload.embeddings.float;
        validate_embedding_count(inputs.len(), embeddings.len())?;
        for embedding in &embeddings {
            validate_embedding_dimension(self.dimensions, embedding)?;
        }
        Ok(embeddings)
    }
}

#[async_trait]
impl EmbeddingProvider for CohereEmbedding {
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

        // Run chunk requests in windows of at most COHERE_EMBED_CONCURRENCY in
        // flight. `try_join_all` resolves each window in request order and the
        // windows run sequentially, so the flattened output stays in input order
        // while still overlapping the HTTP round trips within a window.
        let chunks: Vec<&[String]> = inputs.chunks(COHERE_MAX_TEXTS).collect();
        let mut embeddings = Vec::with_capacity(inputs.len());
        for window in chunks.chunks(COHERE_EMBED_CONCURRENCY) {
            let window_results =
                try_join_all(window.iter().map(|chunk| self.embed_chunk(chunk))).await?;
            for chunk_embeddings in window_results {
                embeddings.extend(chunk_embeddings);
            }
        }
        Ok(embeddings)
    }
}

/// Cohere Embed v4 client configured for graph-memory vector storage.
#[derive(Clone)]
pub struct CohereV4Embedder {
    inner: CohereEmbedding,
}

impl CohereV4Embedder {
    /// Creates a graph-memory Cohere Embed v4 client from an API key.
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        Ok(Self {
            inner: CohereEmbedding::new(api_key, COHERE_DEFAULT_MODEL)?
                .with_dimensions(COHERE_GRAPH_MEMORY_DIMENSIONS)?,
        })
    }

    /// Overrides the Cohere endpoint, primarily for HTTP-level tests.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.inner = self.inner.with_embeddings_url(endpoint);
        self
    }

    /// Overrides the Cohere retrieval role for this embedder.
    #[must_use]
    pub fn with_input_type(mut self, input_type: impl Into<String>) -> Self {
        self.inner = self.inner.with_input_type(input_type);
        self
    }

    /// Overrides the request/input pacing, e.g. to apply trial-tier limits.
    #[must_use]
    pub fn with_rate_limits(mut self, config: PacerConfig) -> Self {
        self.inner = self.inner.with_rate_limits(config);
        self
    }
}

#[async_trait]
impl EmbeddingProvider for CohereV4Embedder {
    fn model_id(&self) -> &str {
        COHERE_DEFAULT_MODEL
    }

    fn model_version(&self) -> i32 {
        1
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }

    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        self.inner.embed(inputs).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct CohereEmbeddingRequest {
    model: String,
    texts: Vec<String>,
    input_type: String,
    embedding_types: Vec<String>,
    output_dimension: usize,
}

#[derive(Debug, Clone, Deserialize)]
struct CohereEmbeddingResponse {
    embeddings: CohereEmbeddings,
}

#[derive(Debug, Clone, Deserialize)]
struct CohereEmbeddings {
    float: Vec<Vec<f32>>,
}

/// Returns the Cohere input type for a graph-memory embedder role.
#[must_use]
pub(super) const fn cohere_input_type_for_role(
    role: super::EmbedderConstructionRole,
) -> &'static str {
    match role {
        super::EmbedderConstructionRole::Ingestion => COHERE_DOCUMENT_INPUT_TYPE,
        super::EmbedderConstructionRole::Retrieval => COHERE_QUERY_INPUT_TYPE,
    }
}

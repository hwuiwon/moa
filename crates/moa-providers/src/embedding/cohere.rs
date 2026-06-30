//! Cohere embedding provider client.

use async_trait::async_trait;
use moa_core::traits::EmbeddingProvider;
use moa_core::{MoaConfig, MoaError, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::core::http::{
    build_http_client, post_json, validate_embedding_count, validate_embedding_dimension,
};

const COHERE_EMBEDDINGS_URL: &str = "https://api.cohere.com/v2/embed";
pub(super) const COHERE_DEFAULT_MODEL: &str = "embed-v4.0";
const COHERE_DEFAULT_INPUT_TYPE: &str = "search_document";
const COHERE_FLOAT_EMBEDDING_TYPE: &str = "float";
const COHERE_MAX_TEXTS: usize = 96;
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
}

impl CohereEmbedding {
    /// Creates a Cohere embedding client from an API key and model id.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Result<Self> {
        Ok(Self {
            client: build_http_client()?,
            api_key: api_key.into(),
            model: model.into(),
            embeddings_url: COHERE_EMBEDDINGS_URL.to_string(),
            input_type: COHERE_DEFAULT_INPUT_TYPE.to_string(),
            dimensions: COHERE_DEFAULT_DIMENSIONS,
        })
    }

    /// Creates a Cohere embedding client from config using an explicit model id.
    pub(super) fn from_config_with_model(config: &MoaConfig, model: String) -> Result<Self> {
        let api_key = moa_core::config::required_config_secret(
            "MOA_COHERE_API_KEY",
            &config.providers.cohere.api_key,
        )?;
        Self::new(api_key, model)
    }

    /// Overrides the embeddings URL, primarily for HTTP-level tests.
    #[must_use]
    pub fn with_embeddings_url(mut self, embeddings_url: impl Into<String>) -> Self {
        self.embeddings_url = embeddings_url.into();
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

        let mut embeddings = Vec::with_capacity(inputs.len());
        for chunk in inputs.chunks(COHERE_MAX_TEXTS) {
            embeddings.extend(self.embed_chunk(chunk).await?);
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

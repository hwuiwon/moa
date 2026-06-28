//! Cohere embedding provider client.

use std::env;

use async_trait::async_trait;
use moa_core::traits::EmbeddingProvider;
use moa_core::{MoaConfig, MoaError, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::model_from_config_with_provider_default;
use crate::core::http::build_http_client;

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

    /// Creates a Cohere embedding client from the loaded MOA config.
    pub fn from_config(config: &MoaConfig) -> Result<Self> {
        Self::from_config_with_env(config, &|name| env::var(name))
    }

    pub(super) fn from_config_with_env(
        config: &MoaConfig,
        _env_lookup: &impl Fn(&str) -> std::result::Result<String, env::VarError>,
    ) -> Result<Self> {
        let api_key = moa_core::config::required_config_secret(
            "MOA_COHERE_API_KEY",
            &config.providers.cohere.api_key,
        )?;
        let model = model_from_config_with_provider_default(config, COHERE_DEFAULT_MODEL);
        Self::from_api_key_and_model(api_key, model)
    }

    pub(super) fn from_config_with_model_env(
        config: &MoaConfig,
        model: String,
        _env_lookup: &impl Fn(&str) -> std::result::Result<String, env::VarError>,
    ) -> Result<Self> {
        let api_key = moa_core::config::required_config_secret(
            "MOA_COHERE_API_KEY",
            &config.providers.cohere.api_key,
        )?;
        Self::from_api_key_and_model(api_key, model)
    }

    fn from_api_key_and_model(api_key: String, model: String) -> Result<Self> {
        Self::new(api_key, model)
    }

    /// Overrides the embeddings URL, primarily for HTTP-level tests.
    #[must_use]
    pub fn with_embeddings_url(mut self, embeddings_url: impl Into<String>) -> Self {
        self.embeddings_url = embeddings_url.into();
        self
    }

    /// Overrides the Cohere input type used for all calls made by this client.
    pub fn with_input_type(mut self, input_type: impl Into<String>) -> Result<Self> {
        let input_type = input_type.into();
        if input_type.trim().is_empty() {
            return Err(MoaError::ConfigError(
                "cohere embedding input_type must not be empty".to_string(),
            ));
        }
        self.input_type = input_type;
        Ok(self)
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
        let response = self
            .client
            .post(&self.embeddings_url)
            .bearer_auth(&self.api_key)
            .json(&CohereEmbeddingRequest {
                model: self.model.clone(),
                texts: inputs.to_vec(),
                input_type: self.input_type.clone(),
                embedding_types: vec![COHERE_FLOAT_EMBEDDING_TYPE.to_string()],
                output_dimension: self.dimensions,
            })
            .send()
            .await
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let message = response
                .text()
                .await
                .unwrap_or_else(|error| format!("failed to read error body: {error}"));
            return Err(MoaError::HttpStatus {
                status: status.as_u16(),
                retry_after: None,
                message,
            });
        }

        let payload: CohereEmbeddingResponse = response
            .json()
            .await
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        let embeddings = payload.embeddings.float;
        if embeddings.len() != inputs.len() {
            return Err(MoaError::ProviderError(format!(
                "embedding response length mismatch: expected {}, got {}",
                inputs.len(),
                embeddings.len()
            )));
        }
        for embedding in &embeddings {
            if embedding.len() != self.dimensions {
                return Err(MoaError::ProviderError(format!(
                    "embedding dimension mismatch: expected {}, got {}",
                    self.dimensions,
                    embedding.len()
                )));
            }
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

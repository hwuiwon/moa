//! OpenAI embedding provider client.

use std::env;

use async_trait::async_trait;
use moa_core::traits::EmbeddingProvider;
use moa_core::{MoaConfig, MoaError, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::core::http::build_http_client;

const OPENAI_EMBEDDINGS_URL: &str = "https://api.openai.com/v1/embeddings";
pub(super) const OPENAI_DEFAULT_MODEL: &str = "text-embedding-3-small";
const OPENAI_DIMENSIONS: usize = 1_536;

/// OpenAI embeddings client backed by the `/v1/embeddings` endpoint.
#[derive(Clone)]
pub struct OpenAIEmbedding {
    client: Client,
    api_key: String,
    model: String,
    embeddings_url: String,
}

impl OpenAIEmbedding {
    /// Creates an OpenAI embedding client from an API key and model id.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Result<Self> {
        Ok(Self {
            client: build_http_client()?,
            api_key: api_key.into(),
            model: model.into(),
            embeddings_url: OPENAI_EMBEDDINGS_URL.to_string(),
        })
    }

    /// Creates an OpenAI embedding client from the loaded MOA config.
    pub fn from_config(config: &MoaConfig) -> Result<Self> {
        Self::from_config_with_env(config, &|name| env::var(name))
    }

    pub(super) fn from_config_with_env(
        config: &MoaConfig,
        _env_lookup: &impl Fn(&str) -> std::result::Result<String, env::VarError>,
    ) -> Result<Self> {
        let api_key = moa_core::config::required_config_secret(
            "MOA_OPENAI_API_KEY",
            &config.providers.openai.api_key,
        )?;
        Self::new(api_key, openai_model_from_config(config))
    }

    /// Overrides the embeddings URL, primarily for HTTP-level tests.
    #[must_use]
    pub fn with_embeddings_url(mut self, embeddings_url: impl Into<String>) -> Self {
        self.embeddings_url = embeddings_url.into();
        self
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

        let response = self
            .client
            .post(&self.embeddings_url)
            .bearer_auth(&self.api_key)
            .json(&OpenAIEmbeddingRequest {
                model: self.model.clone(),
                input: inputs.to_vec(),
                encoding_format: "float".to_string(),
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

        let payload: OpenAIEmbeddingResponse = response
            .json()
            .await
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        if payload.data.len() != inputs.len() {
            return Err(MoaError::ProviderError(format!(
                "embedding response length mismatch: expected {}, got {}",
                inputs.len(),
                payload.data.len()
            )));
        }

        let mut data = payload.data;
        data.sort_by_key(|item| item.index);
        Ok(data.into_iter().map(|item| item.embedding).collect())
    }
}

fn openai_model_from_config(config: &MoaConfig) -> String {
    let model = config.memory.embedding_model.trim();
    if model.is_empty() {
        OPENAI_DEFAULT_MODEL.to_string()
    } else {
        model.to_string()
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

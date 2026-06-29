//! OpenAI embedding provider client.

use async_trait::async_trait;
use moa_core::Result;
use moa_core::traits::EmbeddingProvider;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::core::http::{
    build_http_client, post_json, validate_embedding_count, validate_embedding_dimension,
};

const OPENAI_EMBEDDINGS_URL: &str = "https://api.openai.com/v1/embeddings";
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

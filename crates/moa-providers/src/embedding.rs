//! Embedding providers used by graph memory retrieval.

use std::collections::hash_map::DefaultHasher;
use std::env;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{MoaConfig, MoaError, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::http::build_http_client;

const OPENAI_EMBEDDINGS_URL: &str = "https://api.openai.com/v1/embeddings";
const OPENAI_PROVIDER_NAME: &str = "openai";

/// Shared abstraction over embedding backends used by memory search.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Returns the configured embedding model identifier.
    fn model_id(&self) -> &str;

    /// Returns the fixed dimensionality produced by this embedding model.
    fn dimensions(&self) -> usize;

    /// Computes embeddings for one or more UTF-8 inputs.
    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>>;
}

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
        let api_key_env = config.providers.openai.api_key_env.clone();
        let api_key = env::var(&api_key_env)
            .map_err(|_| MoaError::MissingEnvironmentVariable(api_key_env.clone()))?;
        Self::new(api_key, config.memory.embedding_model.clone())
    }

    /// Overrides the embeddings URL, primarily for HTTP-level tests.
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
        1_536
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

/// Deterministic embedding provider used by tests.
#[derive(Clone, Debug)]
pub struct MockEmbedding {
    dimensions: usize,
    model: String,
}

impl MockEmbedding {
    /// Creates a deterministic mock embedding provider.
    pub fn new(dimensions: usize) -> Self {
        Self {
            dimensions: dimensions.max(8),
            model: format!("mock-embedding-{dimensions}"),
        }
    }

    fn embed_one(&self, input: &str) -> Vec<f32> {
        let mut vector = vec![0.0; self.dimensions];
        let mut token_count = 0_u32;

        for token in tokenize(input) {
            token_count += 1;
            add_feature(&mut vector, &token, 1.0);
            for alias in token_aliases(&token) {
                add_feature(&mut vector, alias, 0.75);
            }
            for trigram in char_trigrams(&token) {
                add_feature(&mut vector, &trigram, 0.2);
            }
        }

        if token_count == 0 {
            vector[0] = 1.0;
            return vector;
        }

        normalize(&mut vector);
        vector
    }
}

#[async_trait]
impl EmbeddingProvider for MockEmbedding {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(inputs.iter().map(|input| self.embed_one(input)).collect())
    }
}

/// Builds the configured embedding provider for semantic memory search.
pub fn build_embedding_provider_from_config(
    config: &MoaConfig,
) -> Result<Option<Arc<dyn EmbeddingProvider>>> {
    match config.memory.embedding_provider.trim() {
        "" | "disabled" => Ok(None),
        OPENAI_PROVIDER_NAME => match OpenAIEmbedding::from_config(config) {
            Ok(provider) => Ok(Some(Arc::new(provider))),
            Err(MoaError::MissingEnvironmentVariable(env_name)) => {
                tracing::warn!(
                    env = %env_name,
                    "semantic memory search disabled because the embedding API key is missing"
                );
                Ok(None)
            }
            Err(error) => Err(error),
        },
        unsupported => Err(MoaError::ConfigError(format!(
            "unsupported memory.embedding_provider '{unsupported}'"
        ))),
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

fn tokenize(input: &str) -> Vec<String> {
    input
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.to_ascii_lowercase())
        .collect()
}

fn char_trigrams(token: &str) -> Vec<String> {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() <= 3 {
        return vec![token.to_string()];
    }

    chars
        .windows(3)
        .map(|window| window.iter().collect::<String>())
        .collect()
}

fn token_aliases(token: &str) -> &'static [&'static str] {
    match token {
        "auth" | "authenticate" | "authentication" => &["oauth", "token", "identity"],
        "identity" => &["auth", "oauth", "token"],
        "oauth" | "oauth2" => &["auth", "token", "refresh"],
        "jwt" => &["token", "auth"],
        "refresh" => &["token", "oauth", "rotation"],
        "rotation" => &["refresh", "token"],
        "token" | "tokens" => &["oauth", "auth", "jwt"],
        "cache" | "caching" => &["reuse", "storage"],
        "replay" => &["history", "session", "events"],
        _ => &[],
    }
}

fn add_feature(vector: &mut [f32], feature: &str, weight: f32) {
    let mut hasher = DefaultHasher::new();
    feature.hash(&mut hasher);
    let idx = (hasher.finish() as usize) % vector.len();
    vector[idx] += weight;
}

fn normalize(vector: &mut [f32]) {
    let norm = vector
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt() as f32;
    if norm > 0.0 {
        for value in vector.iter_mut() {
            *value /= norm;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EmbeddingProvider, MockEmbedding};

    #[tokio::test]
    async fn mock_embedding_is_deterministic() {
        let provider = MockEmbedding::new(64);
        let left = provider
            .embed(&[String::from("oauth refresh token")])
            .await
            .expect("embed");
        let right = provider
            .embed(&[String::from("oauth refresh token")])
            .await
            .expect("embed");

        assert_eq!(left, right);
    }
}

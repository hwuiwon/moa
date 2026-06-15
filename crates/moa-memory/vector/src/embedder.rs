//! Cohere Embed v4 client.

use std::time::Duration;

use async_trait::async_trait;
use backon::{ExponentialBuilder, Retryable};
use reqwest::Client;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use moa_core::traits::EmbeddingProvider;

use crate::{Error, Result, VECTOR_DIMENSION, validate_dimension};

const COHERE_EMBED_URL: &str = "https://api.cohere.com/v2/embed";
const COHERE_MODEL: &str = "embed-v4.0";
const COHERE_MAX_TEXTS: usize = 96;
const RETRY_MIN_DELAY: Duration = Duration::from_millis(200);
const RETRY_ATTEMPTS: usize = 2;

/// Cohere Embed v4 client configured for 1024-dimensional float embeddings.
#[derive(Clone)]
pub struct CohereV4Embedder {
    client: Client,
    api_key: SecretString,
    endpoint: String,
}

impl CohereV4Embedder {
    /// Creates a Cohere Embed v4 client from an API key.
    pub fn new(api_key: SecretString) -> Self {
        Self {
            client: Client::new(),
            api_key,
            endpoint: COHERE_EMBED_URL.to_string(),
        }
    }

    /// Overrides the HTTP client, primarily for tests.
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }

    /// Overrides the Cohere endpoint, primarily for tests.
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    async fn embed_chunk(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let request = CohereEmbedRequest {
            model: COHERE_MODEL,
            texts,
            input_type: "search_document",
            embedding_types: ["float"],
            output_dimension: VECTOR_DIMENSION,
        };
        let backoff = ExponentialBuilder::default()
            .with_min_delay(RETRY_MIN_DELAY)
            .with_max_times(RETRY_ATTEMPTS);

        (|| async { self.embed_chunk_once(&request).await })
            .retry(backoff)
            .when(is_retryable)
            .await
    }

    async fn embed_chunk_once(&self, request: &CohereEmbedRequest<'_>) -> Result<Vec<Vec<f32>>> {
        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(self.api_key.expose_secret())
            .json(request)
            .send()
            .await?;
        let status = response.status();
        if status.is_success() {
            let body = response.json::<CohereEmbedResponse>().await?;
            return Ok(body.embeddings.float);
        }

        let body = match response.text().await {
            Ok(body) => body,
            Err(error) => format!("failed to read error body: {error}"),
        };
        Err(Error::ProviderStatus {
            status: status.as_u16(),
            body,
        })
    }
}

/// Retries only on Cohere rate-limit (429) and server (5xx) responses, matching
/// the previous hand-rolled loop; transport errors propagate immediately.
fn is_retryable(error: &Error) -> bool {
    matches!(
        error,
        Error::ProviderStatus { status, .. } if *status == 429 || (500..=599).contains(status)
    )
}

#[async_trait]
impl EmbeddingProvider for CohereV4Embedder {
    fn model_id(&self) -> &str {
        "cohere-embed-v4"
    }

    fn model_version(&self) -> i32 {
        1
    }

    fn dimensions(&self) -> usize {
        VECTOR_DIMENSION
    }

    async fn embed(&self, texts: &[String]) -> moa_core::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let mut embeddings = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(COHERE_MAX_TEXTS) {
            let chunk_embeddings = self.embed_chunk(chunk).await?;
            if chunk_embeddings.len() != chunk.len() {
                return Err(Error::EmbeddingResponseLength {
                    expected: chunk.len(),
                    actual: chunk_embeddings.len(),
                }
                .into());
            }
            for embedding in &chunk_embeddings {
                validate_dimension(embedding)?;
            }
            embeddings.extend(chunk_embeddings);
        }
        Ok(embeddings)
    }
}

#[derive(Serialize)]
struct CohereEmbedRequest<'a> {
    model: &'static str,
    texts: &'a [String],
    input_type: &'static str,
    embedding_types: [&'static str; 1],
    output_dimension: usize,
}

#[derive(Deserialize)]
struct CohereEmbedResponse {
    embeddings: CohereEmbeddings,
}

#[derive(Deserialize)]
struct CohereEmbeddings {
    float: Vec<Vec<f32>>,
}

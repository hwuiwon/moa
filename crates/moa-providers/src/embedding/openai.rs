//! OpenAI embedding provider client.

use async_trait::async_trait;
use moa_core::error::Result;
use moa_core::traits::EmbeddingProvider;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::core::concurrency::{ConcurrencyLimiter, DEFAULT_MAX_IN_FLIGHT};
use crate::core::http::{
    build_json_http_client, post_json, validate_embedding_count, validate_embedding_dimension,
};
use crate::core::pacer::{PacerConfig, RatePacer};
use crate::core::rate_guard;

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
            limiter: ConcurrencyLimiter::new(DEFAULT_MAX_IN_FLIGHT),
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
    /// Replaces the in-flight concurrency limiter (config-driven or global).
    pub(crate) fn with_limiter(mut self, limiter: ConcurrencyLimiter) -> Self {
        self.limiter = limiter;
        self
    }

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
        let _permit = match self.limiter.acquire().await {
            Some(lease) => lease,
            None => {
                return Err(rate_guard::rate_limited_saturated(
                    self.limiter.block_threshold(),
                ));
            }
        };
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use moa_core::error::MoaError;
    use moa_core::traits::EmbeddingProvider;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    use super::OpenAIEmbedding;

    // Real-time (not paused): the holder keeps its slot by parking on a live,
    // never-responding socket, which reqwest's IO driver cannot drive under a
    // paused clock. The wait is bounded by `DEFAULT_BLOCK_THRESHOLD`.
    #[tokio::test]
    async fn embedding_reports_bounded_error_when_concurrency_gate_saturates() {
        // Pins (F21): an embedding client whose single in-flight slot is already
        // held returns a bounded, retryable RateLimited error once the block
        // threshold elapses, instead of queueing indefinitely ahead of the HTTP
        // timeout.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (ready_tx, ready_rx) = oneshot::channel();

        // Accept the first request, signal that its in-flight slot is held, then
        // keep the connection open (never respond) so the slot stays occupied.
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 2048];
            let _ = socket.read(&mut buffer).await;
            let _ = ready_tx.send(());
            std::future::pending::<()>().await;
        });

        let client = Arc::new(
            OpenAIEmbedding::new("test-key", "text-embedding-3-small")
                .unwrap()
                .with_embeddings_url(format!("http://{address}/v1/embeddings"))
                .with_max_concurrent_requests(1),
        );

        // The first call takes the only slot and parks in the server.
        let holder = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.embed(&["hold the slot".to_string()]).await })
        };
        ready_rx
            .await
            .expect("the first request should reach the server and hold the slot");

        // The second call finds the gate saturated and must fail fast at the
        // threshold rather than waiting on the 60s HTTP timeout.
        let blocked = client.embed(&["blocked".to_string()]).await;
        assert!(
            matches!(blocked, Err(MoaError::RateLimited { .. })),
            "a saturated embedding gate must return a bounded RateLimited error, got {blocked:?}"
        );

        holder.abort();
        server.abort();
    }
}

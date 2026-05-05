//! Reranker clients used after hybrid retrieval fusion.

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use crate::retrieval::hybrid::{Result, RetrievalError};

const COHERE_RERANK_URL: &str = "https://api.cohere.com/v2/rerank";

/// One rerank result with an index into the input document list.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RerankHit {
    /// Candidate index in the supplied document list.
    pub index: usize,
    /// Backend relevance score.
    pub relevance_score: f32,
}

/// Backend abstraction for candidate reranking.
#[async_trait]
pub trait Reranker: Send + Sync {
    /// Reranks document snippets for a query and returns selected indices.
    async fn rerank(
        &self,
        model: &str,
        query: &str,
        documents: &[String],
        top_n: usize,
    ) -> Result<Vec<RerankHit>>;
}

/// Deterministic reranker that preserves the incoming order.
#[derive(Debug, Clone, Default)]
pub struct NoopReranker;

#[async_trait]
impl Reranker for NoopReranker {
    async fn rerank(
        &self,
        _model: &str,
        _query: &str,
        documents: &[String],
        top_n: usize,
    ) -> Result<Vec<RerankHit>> {
        Ok((0..documents.len().min(top_n))
            .map(|index| RerankHit {
                index,
                relevance_score: 1.0,
            })
            .collect())
    }
}

/// Cohere Rerank v4 client.
#[derive(Clone)]
pub struct CohereReranker {
    client: reqwest::Client,
    api_key: SecretString,
    endpoint: String,
}

impl CohereReranker {
    /// Creates a reranker using Cohere's production endpoint.
    #[must_use]
    pub fn new(api_key: SecretString) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            endpoint: COHERE_RERANK_URL.to_string(),
        }
    }

    /// Overrides the HTTP client, primarily for tests.
    #[must_use]
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Overrides the Cohere endpoint, primarily for tests.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }
}

#[async_trait]
impl Reranker for CohereReranker {
    async fn rerank(
        &self,
        model: &str,
        query: &str,
        documents: &[String],
        top_n: usize,
    ) -> Result<Vec<RerankHit>> {
        if documents.is_empty() || top_n == 0 {
            return Ok(Vec::new());
        }

        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(self.api_key.expose_secret())
            .json(&CohereRerankRequest {
                model,
                query,
                documents,
                top_n,
            })
            .send()
            .await
            .map_err(|error| RetrievalError::Rerank(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|error| format!("failed to read error body: {error}"));
            return Err(RetrievalError::Rerank(format!(
                "Cohere rerank returned HTTP {}: {body}",
                status.as_u16()
            )));
        }

        let body = response
            .json::<CohereRerankResponse>()
            .await
            .map_err(|error| RetrievalError::Rerank(error.to_string()))?;
        Ok(body
            .results
            .into_iter()
            .filter(|hit| hit.index < documents.len())
            .map(|hit| RerankHit {
                index: hit.index,
                relevance_score: hit.relevance_score,
            })
            .collect())
    }
}

#[derive(Serialize)]
struct CohereRerankRequest<'a> {
    model: &'a str,
    query: &'a str,
    documents: &'a [String],
    top_n: usize,
}

#[derive(Deserialize)]
struct CohereRerankResponse {
    results: Vec<CohereRerankResponseHit>,
}

#[derive(Deserialize)]
struct CohereRerankResponseHit {
    index: usize,
    relevance_score: f32,
}

#[cfg(test)]
mod tests {
    use super::{NoopReranker, Reranker};

    #[tokio::test]
    async fn noop_reranker_preserves_order_and_limit() {
        let docs = vec!["a".to_string(), "b".to_string(), "c".to_string()];

        let hits = NoopReranker
            .rerank("unused", "query", &docs, 2)
            .await
            .expect("noop rerank should succeed");

        assert_eq!(hits.iter().map(|hit| hit.index).collect::<Vec<_>>(), [0, 1]);
    }
}

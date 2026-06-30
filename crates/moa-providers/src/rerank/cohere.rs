//! Cohere rerank provider client.

use async_trait::async_trait;
use moa_core::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::core::http::{build_http_client, post_json};

use super::{RerankHit, Reranker};

const COHERE_RERANK_URL: &str = "https://api.cohere.com/v2/rerank";
/// Default Cohere Rerank v4 model used for low-latency memory retrieval.
pub const COHERE_DEFAULT_RERANK_MODEL: &str = "rerank-v4.0-fast";

/// Cohere Rerank v4 client.
#[derive(Clone)]
pub struct CohereReranker {
    client: Client,
    api_key: String,
    endpoint: String,
}

impl CohereReranker {
    /// Creates a reranker using Cohere's production endpoint.
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        Ok(Self {
            client: build_http_client()?,
            api_key: api_key.into(),
            endpoint: COHERE_RERANK_URL.to_string(),
        })
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

        let body: CohereRerankResponse = post_json(
            &self.client,
            &self.endpoint,
            &self.api_key,
            &CohereRerankRequest {
                model,
                query,
                documents,
                top_n,
            },
        )
        .await?;
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

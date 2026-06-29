//! ZeroEntropy rerank provider client.

use async_trait::async_trait;
use moa_core::{MoaError, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::core::http::{build_http_client, post_json};

use super::{RerankHit, Reranker};

const ZEROENTROPY_RERANK_URL: &str = "https://api.zeroentropy.dev/v1/models/rerank";
/// Default ZeroEntropy rerank model.
pub const ZEROENTROPY_DEFAULT_RERANK_MODEL: &str = "zerank-2";

/// ZeroEntropy latency mode for rerank calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZeroEntropyRerankLatency {
    /// Force fast inference and fail with 429 when fast capacity is exhausted.
    Fast,
    /// Allow the slower high-throughput inference lane.
    Slow,
}

impl ZeroEntropyRerankLatency {
    pub(super) fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "fast" => Ok(Self::Fast),
            "slow" => Ok(Self::Slow),
            other => Err(MoaError::ConfigError(format!(
                "zeroentropy reranker latency must be fast or slow, got `{other}`"
            ))),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Slow => "slow",
        }
    }
}

/// ZeroEntropy rerank client backed by the `/v1/models/rerank` endpoint.
#[derive(Clone)]
pub struct ZeroEntropyReranker {
    client: Client,
    api_key: String,
    endpoint: String,
    latency: Option<ZeroEntropyRerankLatency>,
}

impl ZeroEntropyReranker {
    /// Creates a ZeroEntropy reranker using the production endpoint.
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        Ok(Self {
            client: build_http_client()?,
            api_key: api_key.into(),
            endpoint: ZEROENTROPY_RERANK_URL.to_string(),
            latency: None,
        })
    }

    /// Overrides the ZeroEntropy endpoint, primarily for tests.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Sets the optional ZeroEntropy latency mode.
    #[must_use]
    pub fn with_latency(mut self, latency: Option<ZeroEntropyRerankLatency>) -> Self {
        self.latency = latency;
        self
    }
}

#[async_trait]
impl Reranker for ZeroEntropyReranker {
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

        let body: ZeroEntropyRerankResponse = post_json(
            &self.client,
            &self.endpoint,
            &self.api_key,
            &ZeroEntropyRerankRequest {
                model,
                query,
                documents,
                top_n: Some(top_n),
                latency: self.latency.map(ZeroEntropyRerankLatency::as_str),
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
struct ZeroEntropyRerankRequest<'a> {
    model: &'a str,
    query: &'a str,
    documents: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    top_n: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    latency: Option<&'static str>,
}

#[derive(Deserialize)]
struct ZeroEntropyRerankResponse {
    results: Vec<ZeroEntropyRerankResponseHit>,
}

#[derive(Deserialize)]
struct ZeroEntropyRerankResponseHit {
    index: usize,
    relevance_score: f32,
}

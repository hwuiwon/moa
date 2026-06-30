//! Gemini embedding provider client.
//!
//! `gemini-embedding-2` is exposed through MOA's existing text-only
//! [`EmbeddingProvider`](moa_core::traits::EmbeddingProvider) trait. The API is
//! multimodal, but binary chunking and sandboxed media handling are out of
//! scope for this provider adapter.

use async_trait::async_trait;
use moa_core::traits::EmbeddingProvider;
use moa_core::{MoaError, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::core::http::{build_http_client, decode_json_response, validate_embedding_dimension};

const GEMINI_ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta";
pub(super) const GEMINI_V2_MODEL: &str = "gemini-embedding-2";

/// Construction role used to pin asymmetric retrieval prefixes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbedderConstructionRole {
    /// Build an ingestion-side document embedder.
    Ingestion,
    /// Build a retrieval-side query embedder.
    Retrieval,
}

/// Task-prefix role for `gemini-embedding-2`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmbedRole {
    /// Document side of asymmetric retrieval.
    Document {
        /// Optional document title; `none` is used when absent.
        title: Option<String>,
    },
    /// Generic search query side.
    SearchQuery,
    /// Question-answering query side.
    QuestionAnsweringQuery,
    /// Fact-checking query side.
    FactCheckingQuery,
    /// Code-retrieval query side.
    CodeRetrievalQuery,
    /// Symmetric classification workload.
    Classification,
    /// Symmetric clustering workload.
    Clustering,
    /// Symmetric sentence-similarity workload.
    SentenceSimilarity,
    /// Pass-through mode for already formatted content.
    Raw,
}

impl EmbedRole {
    /// Formats one text input with the role-specific Gemini v2 prompt prefix.
    #[must_use]
    pub fn format(&self, content: &str) -> String {
        match self {
            Self::Document { title } => {
                format!(
                    "title: {} | text: {content}",
                    title.as_deref().unwrap_or("none")
                )
            }
            Self::SearchQuery => format!("task: search result | query: {content}"),
            Self::QuestionAnsweringQuery => {
                format!("task: question answering | query: {content}")
            }
            Self::FactCheckingQuery => format!("task: fact checking | query: {content}"),
            Self::CodeRetrievalQuery => format!("task: code retrieval | query: {content}"),
            Self::Classification => format!("task: classification | query: {content}"),
            Self::Clustering => format!("task: clustering | query: {content}"),
            Self::SentenceSimilarity => {
                format!("task: sentence similarity | query: {content}")
            }
            Self::Raw => content.to_owned(),
        }
    }
}

/// Gemini text embedder backed by `gemini-embedding-2`.
#[derive(Clone)]
pub struct GeminiEmbeddingEmbedder {
    client: Client,
    api_key: String,
    endpoint: String,
    output_dim: usize,
    default_role: EmbedRole,
}

impl GeminiEmbeddingEmbedder {
    /// Creates a Gemini embedder.
    pub fn new(
        api_key: impl Into<String>,
        output_dim: usize,
        default_role: EmbedRole,
    ) -> Result<Self> {
        validate_gemini_output_dim(output_dim)?;
        Ok(Self {
            client: build_http_client()?,
            api_key: api_key.into(),
            endpoint: GEMINI_ENDPOINT.to_string(),
            output_dim,
            default_role,
        })
    }

    /// Overrides the endpoint base URL, primarily for tests.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Embeds one input with a per-call role override.
    pub async fn embed_as(&self, role: &EmbedRole, text: &str) -> Result<Vec<f32>> {
        let body = V2Request {
            content: GeminiContent {
                parts: vec![GeminiTextPart {
                    text: role.format(text),
                }],
            },
            output_dimensionality: Some(self.output_dim),
        };
        let response = self.post_embed(GEMINI_V2_MODEL, &body).await?;
        validate_embedding_dimension(self.output_dim, &response.embedding.values)?;
        Ok(response.embedding.values)
    }

    async fn post_embed<T: Serialize>(&self, model: &str, body: &T) -> Result<GeminiResponse> {
        let response = self
            .client
            .post(format!(
                "{}/models/{model}:embedContent",
                self.endpoint.trim_end_matches('/')
            ))
            .header("x-goog-api-key", &self.api_key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(body)
            .send()
            .await
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        decode_json_response(response).await
    }
}

#[async_trait]
impl EmbeddingProvider for GeminiEmbeddingEmbedder {
    fn model_id(&self) -> &str {
        GEMINI_V2_MODEL
    }

    fn model_version(&self) -> i32 {
        2
    }

    fn dimensions(&self) -> usize {
        self.output_dim
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            out.push(self.embed_as(&self.default_role, text).await?);
        }
        Ok(out)
    }
}

#[derive(Serialize)]
struct V2Request {
    content: GeminiContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_dimensionality: Option<usize>,
}

#[derive(Serialize)]
struct GeminiContent {
    parts: Vec<GeminiTextPart>,
}

#[derive(Serialize)]
struct GeminiTextPart {
    text: String,
}

#[derive(Deserialize)]
struct GeminiResponse {
    embedding: GeminiEmbedding,
}

#[derive(Deserialize)]
struct GeminiEmbedding {
    values: Vec<f32>,
}

fn validate_gemini_output_dim(output_dim: usize) -> Result<()> {
    if (128..=3072).contains(&output_dim) {
        Ok(())
    } else {
        Err(MoaError::ConfigError(format!(
            "Gemini output_dim must be in 128..=3072, got {output_dim}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::EmbedRole;

    #[test]
    fn role_prefixes_match_documented_shapes() {
        assert!(
            EmbedRole::SearchQuery
                .format("oauth")
                .starts_with("task: search result | query: ")
        );
        assert!(
            EmbedRole::Document { title: None }
                .format("oauth")
                .starts_with("title: none | text: ")
        );
        assert!(
            EmbedRole::QuestionAnsweringQuery
                .format("oauth")
                .starts_with("task: question answering | query: ")
        );
        assert!(
            EmbedRole::FactCheckingQuery
                .format("oauth")
                .starts_with("task: fact checking | query: ")
        );
        assert!(
            EmbedRole::CodeRetrievalQuery
                .format("oauth")
                .starts_with("task: code retrieval | query: ")
        );
        assert!(
            EmbedRole::Classification
                .format("oauth")
                .starts_with("task: classification | query: ")
        );
        assert!(
            EmbedRole::Clustering
                .format("oauth")
                .starts_with("task: clustering | query: ")
        );
        assert!(
            EmbedRole::SentenceSimilarity
                .format("oauth")
                .starts_with("task: sentence similarity | query: ")
        );
        assert_eq!(EmbedRole::Raw.format("oauth"), "oauth");
    }
}

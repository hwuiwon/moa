//! Gemini embedding provider clients.
//!
//! `gemini-embedding-2` is exposed through MOA's existing text-only
//! [`Embedder`] trait. The API is multimodal, but binary chunking and sandboxed
//! media handling are out of scope for this layer.

use std::sync::Arc;

use async_trait::async_trait;
use moa_core::MoaConfig;
use reqwest::Client;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use crate::{CohereV4Embedder, Embedder, Error, Result};

const GEMINI_ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta";
const GEMINI_V2_MODEL: &str = "gemini-embedding-2";

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
    api_key: SecretString,
    endpoint: String,
    output_dim: usize,
    default_role: EmbedRole,
}

impl GeminiEmbeddingEmbedder {
    /// Creates a Gemini embedder.
    pub fn new(api_key: SecretString, output_dim: usize, default_role: EmbedRole) -> Result<Self> {
        validate_gemini_output_dim(output_dim)?;
        Ok(Self {
            client: Client::new(),
            api_key,
            endpoint: GEMINI_ENDPOINT.to_string(),
            output_dim,
            default_role,
        })
    }

    /// Overrides the HTTP client, primarily for tests.
    #[must_use]
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
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
        validate_gemini_dimension(self.output_dim, &response.embedding.values)?;
        Ok(response.embedding.values)
    }

    async fn post_embed<T: Serialize>(&self, model: &str, body: &T) -> Result<GeminiResponse> {
        let response = self
            .client
            .post(format!(
                "{}/models/{model}:embedContent",
                self.endpoint.trim_end_matches('/')
            ))
            .header("x-goog-api-key", self.api_key.expose_secret())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(body)
            .send()
            .await?;
        let status = response.status();
        if status.is_success() {
            return Ok(response.json::<GeminiResponse>().await?);
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

#[async_trait]
impl Embedder for GeminiEmbeddingEmbedder {
    fn model_name(&self) -> &'static str {
        GEMINI_V2_MODEL
    }

    fn model_version(&self) -> i32 {
        2
    }

    fn dimension(&self) -> usize {
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

/// Builds a graph-memory embedder from MOA config and a construction role.
pub fn build_embedder_from_config(
    config: &MoaConfig,
    role: EmbedderConstructionRole,
) -> Result<Arc<dyn Embedder>> {
    let cfg = &config.memory.vector.embedder;
    match cfg.name.as_str() {
        "cohere-embed-v4" => {
            let api_key = read_secret_env(&cfg.cohere.api_key_env)?;
            Ok(Arc::new(CohereV4Embedder::new(api_key)))
        }
        "gemini-embedding-2" => {
            let api_key = read_secret_env(&cfg.gemini.api_key_env)?;
            let role = match role {
                EmbedderConstructionRole::Ingestion => EmbedRole::Document { title: None },
                EmbedderConstructionRole::Retrieval => parse_embed_role(&cfg.gemini.default_role)?,
            };
            Ok(Arc::new(GeminiEmbeddingEmbedder::new(
                api_key,
                cfg.output_dim,
                role,
            )?))
        }
        other => Err(Error::EmbedderConfig(format!("unknown embedder `{other}`"))),
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

fn read_secret_env(name: &str) -> Result<SecretString> {
    std::env::var(name)
        .map(SecretString::from)
        .map_err(|error| {
            Error::EmbedderConfig(format!(
                "failed to read embedder API key env `{name}`: {error}"
            ))
        })
}

fn parse_embed_role(value: &str) -> Result<EmbedRole> {
    match normalize_config_key(value).as_str() {
        "search_query" => Ok(EmbedRole::SearchQuery),
        "document" => Ok(EmbedRole::Document { title: None }),
        "question_answering" => Ok(EmbedRole::QuestionAnsweringQuery),
        "fact_checking" => Ok(EmbedRole::FactCheckingQuery),
        "code_retrieval" => Ok(EmbedRole::CodeRetrievalQuery),
        "classification" => Ok(EmbedRole::Classification),
        "clustering" => Ok(EmbedRole::Clustering),
        "sentence_similarity" => Ok(EmbedRole::SentenceSimilarity),
        "raw" => Ok(EmbedRole::Raw),
        other => Err(Error::EmbedderConfig(format!(
            "unknown gemini v2 embed role `{other}`"
        ))),
    }
}

fn normalize_config_key(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace('-', "_")
}

fn validate_gemini_output_dim(output_dim: usize) -> Result<()> {
    if (128..=3072).contains(&output_dim) {
        Ok(())
    } else {
        Err(Error::EmbedderConfig(format!(
            "Gemini output_dim must be in 128..=3072, got {output_dim}"
        )))
    }
}

fn validate_gemini_dimension(expected: usize, embedding: &[f32]) -> Result<()> {
    if embedding.len() == expected {
        Ok(())
    } else {
        Err(Error::DimensionMismatch {
            expected,
            actual: embedding.len(),
        })
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

    #[test]
    fn v2_keeps_already_normalized_values_unchanged() {
        let values = vec![0.6_f32, 0.8_f32];
        assert_eq!(values, vec![0.6, 0.8]);
    }

    #[test]
    fn vector_dimension_constant_still_matches_cohere_default() {
        assert_eq!(crate::VECTOR_DIMENSION, 1024);
    }
}

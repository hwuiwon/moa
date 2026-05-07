//! Memory subsystem and embedding configuration.

use serde::{Deserialize, Serialize};

/// Memory bootstrap and maintenance configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Automatically bootstrap workspace memory when it is empty.
    pub auto_bootstrap: bool,
    /// Embedding provider used for graph memory retrieval. Set to `disabled` to turn it off.
    pub embedding_provider: String,
    /// Embedding model identifier used for graph memory embedding backfills and queries.
    pub embedding_model: String,
    /// Graph-memory vector embedding configuration.
    pub vector: MemoryVectorConfig,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            auto_bootstrap: true,
            embedding_provider: "openai".to_string(),
            embedding_model: "text-embedding-3-small".to_string(),
            vector: MemoryVectorConfig::default(),
        }
    }
}

/// Graph-memory vector configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryVectorConfig {
    /// Embedder selection and credentials.
    pub embedder: VectorEmbedderConfig,
}

/// Per-workspace embedder selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VectorEmbedderConfig {
    /// Embedder model name.
    pub name: String,
    /// Requested output dimensionality.
    pub output_dim: usize,
    /// Cohere-specific settings.
    pub cohere: CohereEmbedderConfig,
    /// Gemini-specific settings.
    pub gemini: GeminiEmbedderConfig,
}

impl Default for VectorEmbedderConfig {
    fn default() -> Self {
        Self {
            name: "gemini-embedding-2".to_string(),
            output_dim: 1024,
            cohere: CohereEmbedderConfig::default(),
            gemini: GeminiEmbedderConfig::default(),
        }
    }
}

/// Cohere embedder credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CohereEmbedderConfig {
    /// Environment variable containing the Cohere API key.
    pub api_key_env: String,
}

impl Default for CohereEmbedderConfig {
    fn default() -> Self {
        Self {
            api_key_env: "COHERE_API_KEY".to_string(),
        }
    }
}

/// Gemini embedder credentials and task encoding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiEmbedderConfig {
    /// Environment variable containing the Gemini API key.
    pub api_key_env: String,
    /// Default Gemini v2 role for retrieval-side embedders.
    pub default_role: String,
}

impl Default for GeminiEmbedderConfig {
    fn default() -> Self {
        Self {
            api_key_env: "GEMINI_API_KEY".to_string(),
            default_role: "search_query".to_string(),
        }
    }
}

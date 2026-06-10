//! Memory subsystem and embedding configuration.

use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// Memory bootstrap and maintenance configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Automatically bootstrap workspace memory when it is empty.
    pub auto_bootstrap: bool,
    /// Optional HTTP base URL for the PII classification sidecar.
    pub pii_service_url: Option<String>,
    /// Embedding provider used for graph memory retrieval. Set to `disabled` to turn it off.
    pub embedding_provider: String,
    /// Embedding model identifier used for graph memory embedding backfills and queries.
    pub embedding_model: String,
    /// Graph-memory retrieval behavior.
    pub retrieval: MemoryRetrievalConfig,
    /// Graph-memory vector embedding configuration.
    pub vector: MemoryVectorConfig,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            auto_bootstrap: true,
            pii_service_url: None,
            embedding_provider: "openai".to_string(),
            embedding_model: "text-embedding-3-small".to_string(),
            retrieval: MemoryRetrievalConfig::default(),
            vector: MemoryVectorConfig::default(),
        }
    }
}

/// Graph-memory retrieval configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryRetrievalConfig {
    /// Runtime/eval mode for post-fusion memory reranking.
    pub reranker_mode: MemoryRerankerMode,
    /// Deterministic post-hydration ranking behavior.
    pub ranking: MemoryRankingConfig,
}

/// Deterministic ranking configuration for graph-memory retrieval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryRankingConfig {
    /// Ranking mode.
    pub mode: MemoryRankingMode,
    /// Feature weights used by FeatureV1.
    pub weights: MemoryRankingWeights,
}

impl Default for MemoryRankingConfig {
    fn default() -> Self {
        Self {
            mode: MemoryRankingMode::FeatureV1,
            weights: MemoryRankingWeights::default(),
        }
    }
}

/// Ranking mode for graph-memory retrieval.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryRankingMode {
    /// Preserve the legacy RRF plus layer-bias ranking path.
    Legacy,
    /// Apply deterministic feature scoring after candidate hydration.
    #[default]
    FeatureV1,
}

impl FromStr for MemoryRankingMode {
    type Err = MemoryRankingModeParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "legacy" => Ok(Self::Legacy),
            "feature_v1" => Ok(Self::FeatureV1),
            _ => Err(MemoryRankingModeParseError),
        }
    }
}

/// Parse error for memory ranking mode strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryRankingModeParseError;

impl std::fmt::Display for MemoryRankingModeParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("expected one of legacy, feature_v1")
    }
}

/// Weights used by deterministic graph-memory ranking.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryRankingWeights {
    /// Normalized reciprocal-rank fusion contribution.
    pub rrf: f64,
    /// Valid-from recency contribution.
    pub recency: f64,
    /// Last-access recency contribution.
    pub access: f64,
    /// Exact subject-token match contribution.
    pub subject_match: f64,
    /// Query-to-summary token overlap contribution.
    pub overlap: f64,
    /// Additive score for user-scoped rows.
    pub scope_user: f64,
    /// Additive score for workspace-scoped rows.
    pub scope_workspace: f64,
    /// Half-life in days for valid-from recency.
    pub recency_half_life_days: f64,
    /// Half-life in days for access recency.
    pub access_half_life_days: f64,
}

impl Default for MemoryRankingWeights {
    fn default() -> Self {
        Self {
            rrf: 1.0,
            recency: 0.3,
            access: 0.15,
            subject_match: 0.5,
            overlap: 0.35,
            scope_user: 0.2,
            scope_workspace: 0.1,
            recency_half_life_days: 90.0,
            access_half_life_days: 14.0,
        }
    }
}

/// Reranker enablement mode for graph-memory retrieval.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryRerankerMode {
    /// Do not apply reranking.
    #[default]
    Off,
    /// Keep runtime reranking disabled while allowing eval jobs to opt in.
    EvalOnly,
    /// Apply reranking in the normal runtime retrieval pipeline.
    On,
}

impl FromStr for MemoryRerankerMode {
    type Err = MemoryRerankerModeParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "eval_only" => Ok(Self::EvalOnly),
            "on" => Ok(Self::On),
            _ => Err(MemoryRerankerModeParseError),
        }
    }
}

/// Parse error for memory reranker mode strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryRerankerModeParseError;

impl std::fmt::Display for MemoryRerankerModeParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("expected one of off, eval_only, on")
    }
}

/// Graph-memory vector configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryVectorConfig {
    /// Embedder selection and credentials.
    pub embedder: VectorEmbedderConfig,
    /// Optional Turbopuffer backend configuration.
    pub turbopuffer: TurbopufferVectorConfig,
}

/// Turbopuffer graph-memory vector backend configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TurbopufferVectorConfig {
    /// Environment variable containing the Turbopuffer API key.
    pub api_key_env: String,
    /// Optional Turbopuffer API base URL override.
    pub base_url: Option<String>,
    /// Optional namespace environment segment.
    pub environment: Option<String>,
    /// Whether the configured Turbopuffer account has a BAA for restricted data.
    pub baa_enabled: bool,
}

impl Default for TurbopufferVectorConfig {
    fn default() -> Self {
        Self {
            api_key_env: "TURBOPUFFER_API_KEY".to_string(),
            base_url: None,
            environment: None,
            baa_enabled: false,
        }
    }
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

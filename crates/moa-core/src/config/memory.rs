//! Memory subsystem and embedding configuration.

use serde::{Deserialize, Serialize};

/// Memory bootstrap and maintenance configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Automatically bootstrap tenant-visible memory when it is empty.
    pub auto_bootstrap: bool,
    /// Optional HTTP base URL for the PII classification sidecar.
    pub pii_service_url: Option<String>,
    /// Embedding provider used for graph memory retrieval. Set to `disabled` to turn it off.
    pub embedding_provider: String,
    /// Embedding model identifier used for graph memory embedding backfills and queries.
    pub embedding_model: String,
    /// Graph-memory retrieval behavior.
    pub retrieval: MemoryRetrievalConfig,
    /// Fact extraction behavior for slow-path graph ingestion.
    pub extraction: MemoryExtractionConfig,
    /// Graph-memory vector embedding configuration.
    pub vector: MemoryVectorConfig,
    /// Standing memory digest rebuild and injection configuration.
    pub digest: MemoryDigestConfig,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            auto_bootstrap: true,
            pii_service_url: None,
            embedding_provider: "openai".to_string(),
            embedding_model: "text-embedding-3-small".to_string(),
            retrieval: MemoryRetrievalConfig::default(),
            extraction: MemoryExtractionConfig::default(),
            vector: MemoryVectorConfig::default(),
            digest: MemoryDigestConfig::default(),
        }
    }
}

/// Standing contact/tenant digest configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryDigestConfig {
    /// Whether the brain context pipeline injects stored digest rows.
    pub enabled: bool,
    /// Maximum rendered digest size using the rough chars/4 token estimate.
    pub max_tokens: usize,
    /// Minimum interval between digest row rebuilds during consolidation.
    pub rebuild_min_interval_hours: i64,
}

impl Default for MemoryDigestConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_tokens: 600,
            rebuild_min_interval_hours: 6,
        }
    }
}

/// Slow-path fact extraction configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryExtractionConfig {
    /// Whether model-backed fact extraction is enabled.
    pub enabled: bool,
    /// Environment variable containing the Cohere API key.
    pub api_key_env: String,
    /// Cohere chat model used for extraction and memory-ingest chat judging.
    pub model: String,
    /// Maximum facts accepted from one chunk.
    pub max_facts_per_chunk: usize,
    /// Chat request timeout in milliseconds.
    pub timeout_ms: u64,
}

impl Default for MemoryExtractionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key_env: "COHERE_API_KEY".to_string(),
            model: "command-a-plus-05-2026".to_string(),
            max_facts_per_chunk: 12,
            timeout_ms: 10_000,
        }
    }
}

/// Graph-memory retrieval configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryRetrievalConfig {
    /// Runtime/eval mode for post-fusion memory reranking.
    pub reranker_mode: MemoryRerankerMode,
    /// Whether retrieval writes narrow quality-scoring lineage rows.
    pub lineage_enabled: bool,
    /// Deterministic post-hydration ranking behavior.
    pub ranking: MemoryRankingConfig,
}

/// Deterministic ranking configuration for graph-memory retrieval.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryRankingConfig {
    /// Feature weights used by deterministic post-hydration ranking.
    pub weights: MemoryRankingWeights,
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
    /// Rescue bonus for candidates only graph expansion found.
    pub graph_rescue: f64,
    /// Outcome-derived quality prior contribution.
    pub quality: f64,
    /// Additive score for contact-scoped rows.
    pub scope_user: f64,
    /// Additive score for tenant-scoped rows.
    pub scope_tenant: f64,
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
            graph_rescue: 0.6,
            quality: 0.6,
            scope_user: 0.2,
            scope_tenant: 0.1,
            recency_half_life_days: 90.0,
            access_half_life_days: 14.0,
        }
    }
}

/// Reranker enablement mode for graph-memory retrieval.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, strum::EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum MemoryRerankerMode {
    /// Do not apply reranking.
    #[default]
    Off,
    /// Keep runtime reranking disabled while allowing eval jobs to opt in.
    EvalOnly,
    /// Apply reranking in the normal runtime retrieval pipeline.
    On,
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

/// Per-tenant embedder selection.
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

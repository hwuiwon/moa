//! Memory subsystem and embedding configuration.

use serde::{Deserialize, Serialize};

/// Memory bootstrap and maintenance configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Optional HTTP base URL for the PII classification sidecar.
    pub pii_service_url: Option<String>,
    /// Embedding model selector used for graph memory embedding backfills and queries.
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
            pii_service_url: None,
            embedding_model: "openai:text-embedding-3-small".to_string(),
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
    /// Cohere API key value loaded from runtime configuration.
    pub api_key: String,
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
            api_key: String::new(),
            model: "command-a-plus-05-2026".to_string(),
            max_facts_per_chunk: 12,
            timeout_ms: 10_000,
        }
    }
}

/// Graph-memory retrieval configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryRetrievalConfig {
    /// Reranker model selector. `noop` preserves the fused order.
    pub reranker_model: String,
    /// Optional provider-specific reranker latency mode.
    pub reranker_latency: Option<String>,
    /// Whether retrieval writes narrow quality-scoring lineage rows.
    pub lineage_enabled: bool,
    /// Deterministic post-hydration ranking behavior.
    pub ranking: MemoryRankingConfig,
}

impl Default for MemoryRetrievalConfig {
    fn default() -> Self {
        Self {
            reranker_model: "noop".to_string(),
            reranker_latency: None,
            lineage_enabled: false,
            ranking: MemoryRankingConfig::default(),
        }
    }
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TurbopufferVectorConfig {
    /// Turbopuffer API key value loaded from runtime configuration.
    pub api_key: String,
    /// Optional Turbopuffer API base URL override.
    pub base_url: Option<String>,
    /// Optional namespace environment segment.
    pub environment: Option<String>,
    /// Whether the configured Turbopuffer account has a BAA for restricted data.
    pub baa_enabled: bool,
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
    /// ZeroEntropy-specific settings.
    pub zeroentropy: ZeroEntropyEmbedderConfig,
    /// Gemini-specific settings.
    pub gemini: GeminiEmbedderConfig,
}

impl Default for VectorEmbedderConfig {
    fn default() -> Self {
        Self {
            name: "gemini:gemini-embedding-2".to_string(),
            output_dim: 1024,
            cohere: CohereEmbedderConfig::default(),
            zeroentropy: ZeroEntropyEmbedderConfig::default(),
            gemini: GeminiEmbedderConfig::default(),
        }
    }
}

/// Cohere embedder credentials.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CohereEmbedderConfig {
    /// Cohere API key value loaded from runtime configuration.
    pub api_key: String,
}

/// ZeroEntropy embedder credentials.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ZeroEntropyEmbedderConfig {
    /// ZeroEntropy API key value loaded from runtime configuration.
    pub api_key: String,
}

/// Gemini embedder credentials and task encoding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiEmbedderConfig {
    /// Gemini API key value loaded from runtime configuration.
    pub api_key: String,
    /// Default Gemini v2 role for retrieval-side embedders.
    pub default_role: String,
}

impl Default for GeminiEmbedderConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            default_role: "search_query".to_string(),
        }
    }
}

impl super::MoaEnvOverlay {
    /// Applies memory, extraction, retrieval, digest, and vector environment overrides.
    pub(in crate::config) fn apply_memory_overlay(&self, config: &mut super::MoaConfig) {
        use super::env_overlay::{set_copy_if_some, set_if_some, set_option_if_some};

        set_if_some(
            &mut config.memory.embedding_model,
            &self.memory_embedding_model,
        );
        set_if_some(
            &mut config.memory.retrieval.reranker_model,
            &self.memory_retrieval_reranker_model,
        );
        set_option_if_some(
            &mut config.memory.retrieval.reranker_latency,
            &self.memory_retrieval_reranker_latency,
        );
        set_copy_if_some(
            &mut config.memory.retrieval.lineage_enabled,
            self.memory_retrieval_lineage_enabled,
        );
        set_copy_if_some(
            &mut config.memory.digest.enabled,
            self.memory_digest_enabled,
        );
        set_copy_if_some(
            &mut config.memory.digest.max_tokens,
            self.memory_digest_max_tokens,
        );
        set_copy_if_some(
            &mut config.memory.digest.rebuild_min_interval_hours,
            self.memory_digest_rebuild_min_interval_hours,
        );
        set_copy_if_some(
            &mut config.memory.extraction.enabled,
            self.memory_extraction_enabled,
        );
        set_if_some(
            &mut config.memory.extraction.model,
            &self.memory_extraction_model,
        );
        set_copy_if_some(
            &mut config.memory.extraction.max_facts_per_chunk,
            self.memory_extraction_max_facts_per_chunk,
        );
        set_copy_if_some(
            &mut config.memory.extraction.timeout_ms,
            self.memory_extraction_timeout_ms,
        );
        set_if_some(
            &mut config.memory.vector.embedder.name,
            &self.memory_vector_embedder_name,
        );
        set_copy_if_some(
            &mut config.memory.vector.embedder.output_dim,
            self.memory_vector_embedder_output_dim,
        );
        set_if_some(
            &mut config.memory.vector.embedder.gemini.default_role,
            &self.memory_vector_embedder_gemini_default_role,
        );
        set_option_if_some(&mut config.memory.pii_service_url, &self.pii_service_url);
        set_if_some(
            &mut config.memory.vector.turbopuffer.api_key,
            &self.turbopuffer_api_key,
        );
        set_option_if_some(
            &mut config.memory.vector.turbopuffer.base_url,
            &self.turbopuffer_base_url,
        );
        set_option_if_some(
            &mut config.memory.vector.turbopuffer.environment,
            &self.turbopuffer_environment,
        );
        set_copy_if_some(
            &mut config.memory.vector.turbopuffer.baa_enabled,
            self.turbopuffer_baa,
        );
    }
}

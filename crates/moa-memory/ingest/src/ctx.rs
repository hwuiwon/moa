//! Runtime context installed by hosts that execute graph-memory ingestion.

use std::sync::{Arc, OnceLock};

use moa_core::{MoaConfig, traits::EmbeddingProvider};
use moa_memory_graph::GraphStore;
use moa_memory_pii::PiiClassifier;
use moa_memory_vector::VectorStore;
use sqlx::PgPool;

use crate::{ContradictionDetector, IngestError, Result};

static INGEST_RUNTIME: OnceLock<IngestRuntime> = OnceLock::new();

/// Scope-specific dependencies used by ingestion helpers.
#[derive(Clone)]
pub struct IngestCtx {
    /// Graph store used for atomic graph writes.
    pub graph: Arc<dyn GraphStore>,
    /// Vector store used for candidate retrieval and vector writes.
    pub vector: Arc<dyn VectorStore>,
    /// Embedder used by ingestion paths that produce vectors.
    pub embedder: Arc<dyn EmbeddingProvider>,
    /// PII classifier used before graph writes.
    pub pii: Arc<dyn PiiClassifier>,
    /// Contradiction detector shared by slow and fast ingestion.
    pub contradict: Arc<dyn ContradictionDetector>,
    /// Postgres pool used for sidecar and dedup queries.
    pub pool: PgPool,
}

impl IngestCtx {
    /// Creates an ingestion context from explicit dependencies.
    #[must_use]
    pub fn new(
        pool: PgPool,
        graph: Arc<dyn GraphStore>,
        vector: Arc<dyn VectorStore>,
        embedder: Arc<dyn EmbeddingProvider>,
        pii: Arc<dyn PiiClassifier>,
        contradict: Arc<dyn ContradictionDetector>,
    ) -> Self {
        Self {
            graph,
            vector,
            embedder,
            pii,
            contradict,
            pool,
        }
    }
}

/// Process-local runtime inputs needed by Restate ingestion handlers.
#[derive(Clone)]
pub struct IngestRuntime {
    pool: PgPool,
    pii_service_url: Option<String>,
    cohere_api_key_env: String,
}

impl IngestRuntime {
    /// Creates a runtime from a Postgres pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        let default_config = MoaConfig::default();
        Self {
            pool,
            pii_service_url: None,
            cohere_api_key_env: default_config.memory.vector.embedder.cohere.api_key_env,
        }
    }

    /// Creates a runtime from a Postgres pool and shared MOA config.
    #[must_use]
    pub fn from_config(pool: PgPool, config: &MoaConfig) -> Self {
        Self {
            pool,
            pii_service_url: config.memory.pii_service_url.clone(),
            cohere_api_key_env: config.memory.vector.embedder.cohere.api_key_env.clone(),
        }
    }

    /// Returns the Postgres pool used by ingestion handlers.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Returns the configured PII classifier sidecar URL.
    #[must_use]
    pub fn pii_service_url(&self) -> Option<&str> {
        self.pii_service_url.as_deref()
    }

    /// Returns the configured Cohere API-key environment variable name.
    #[must_use]
    pub fn cohere_api_key_env(&self) -> &str {
        &self.cohere_api_key_env
    }
}

/// Installs the process-local ingestion runtime.
pub fn install_runtime(runtime: IngestRuntime) -> std::result::Result<(), IngestRuntime> {
    INGEST_RUNTIME.set(runtime)
}

/// Installs the process-local ingestion runtime from a Postgres pool.
pub fn install_runtime_with_pool(pool: PgPool) -> std::result::Result<(), IngestRuntime> {
    install_runtime(IngestRuntime::new(pool))
}

/// Installs the process-local ingestion runtime from a Postgres pool and shared config.
pub fn install_runtime_with_config(
    pool: PgPool,
    config: &MoaConfig,
) -> std::result::Result<(), IngestRuntime> {
    install_runtime(IngestRuntime::from_config(pool, config))
}

/// Returns the installed process-local ingestion runtime.
pub fn current_runtime() -> Result<IngestRuntime> {
    INGEST_RUNTIME
        .get()
        .cloned()
        .ok_or(IngestError::RuntimeNotInstalled)
}

//! Shared runtime resources for the Restate-backed orchestrator binary.

use std::sync::{Arc, OnceLock};

use moa_core::{MemoryStore, MoaConfig};
use moa_session::PostgresSessionStore;
use serde_json::Value;
use sqlx::PgPool;

use crate::services::llm_gateway::ProviderRegistry;

/// Shared Postgres pool initialized at binary startup for Restate handlers.
pub static POOL: OnceLock<PgPool> = OnceLock::new();

/// Shared runtime configuration initialized at binary startup for Restate handlers.
pub static CONFIG: OnceLock<Arc<MoaConfig>> = OnceLock::new();

/// Shared provider registry initialized at binary startup for Restate handlers.
pub static PROVIDERS: OnceLock<Arc<ProviderRegistry>> = OnceLock::new();

/// Shared session-store backend initialized at binary startup for Restate handlers.
pub static SESSION_STORE: OnceLock<Arc<PostgresSessionStore>> = OnceLock::new();

/// Shared memory-store backend initialized at binary startup for Restate handlers.
pub static MEMORY_STORE: OnceLock<Arc<dyn MemoryStore>> = OnceLock::new();

/// Shared compiled tool schemas initialized at binary startup for Restate handlers.
pub static TOOL_SCHEMAS: OnceLock<Arc<Vec<Value>>> = OnceLock::new();

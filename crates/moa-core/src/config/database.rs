//! Database connection and Neon checkpoint configuration.

use serde::{Deserialize, Serialize};

/// Session database configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DatabaseConfig {
    /// Runtime Postgres connection URL.
    pub url: String,
    /// Optional direct/admin database URL for migrations and other session-sensitive flows.
    pub admin_url: Option<String>,
    /// Optional Postgres schema name for isolated runtime stores.
    pub schema: Option<String>,
    /// Maximum pool size for the shared Postgres client.
    pub max_connections: u32,
    /// Connection timeout in seconds.
    pub connect_timeout_seconds: u64,
    /// Optional Neon branching configuration for ephemeral checkpoints.
    pub neon: DatabaseNeonConfig,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: "postgres://moa_owner:dev@localhost:10040/moa".to_string(),
            admin_url: None,
            schema: None,
            max_connections: 20,
            connect_timeout_seconds: 10,
            neon: DatabaseNeonConfig::default(),
        }
    }
}

impl DatabaseConfig {
    /// Returns the configured runtime database URL.
    pub fn runtime_url(&self) -> &str {
        &self.url
    }

    /// Returns the direct/admin database URL, falling back to the runtime URL when unset.
    pub fn admin_url(&self) -> &str {
        self.admin_url.as_deref().unwrap_or(&self.url)
    }
}

/// Optional Neon branching configuration for ephemeral database checkpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DatabaseNeonConfig {
    /// Whether Neon checkpoint management is enabled.
    pub enabled: bool,
    /// Environment variable containing the Neon API key.
    pub api_key_env: String,
    /// Neon project identifier used for branch management.
    pub project_id: String,
    /// Parent branch name or id used for checkpoint creation.
    pub parent_branch_id: String,
    /// Maximum number of active MOA checkpoint branches.
    pub max_checkpoints: usize,
    /// TTL for automatic checkpoint cleanup, in hours.
    pub checkpoint_ttl_hours: u64,
    /// Whether pooled connection URIs should be requested for checkpoint branches.
    pub pooled: bool,
    /// Auto-suspend timeout in seconds for checkpoint endpoints.
    pub suspend_timeout_seconds: u64,
}

impl Default for DatabaseNeonConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key_env: "NEON_API_KEY".to_string(),
            project_id: String::new(),
            parent_branch_id: "main".to_string(),
            max_checkpoints: 5,
            checkpoint_ttl_hours: 24,
            pooled: true,
            suspend_timeout_seconds: 300,
        }
    }
}

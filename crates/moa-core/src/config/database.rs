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
    /// Neon API key value loaded from runtime configuration.
    pub api_key: String,
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
            api_key: String::new(),
            project_id: String::new(),
            parent_branch_id: "main".to_string(),
            max_checkpoints: 5,
            checkpoint_ttl_hours: 24,
            pooled: true,
            suspend_timeout_seconds: 300,
        }
    }
}

impl super::MoaEnvOverlay {
    /// Applies database and Neon checkpoint environment overrides.
    pub(in crate::config) fn apply_database_overlay(&self, config: &mut super::MoaConfig) {
        use super::env_overlay::{set_copy_if_some, set_if_some, set_option_if_some};

        set_if_some(&mut config.database.url, &self.database_url);
        set_option_if_some(&mut config.database.admin_url, &self.database_admin_url);
        set_option_if_some(&mut config.database.schema, &self.database_schema);
        set_copy_if_some(
            &mut config.database.max_connections,
            self.database_max_connections,
        );
        set_copy_if_some(
            &mut config.database.connect_timeout_seconds,
            self.database_connect_timeout_seconds,
        );
        set_copy_if_some(
            &mut config.database.neon.enabled,
            self.database_neon_enabled,
        );
        set_if_some(
            &mut config.database.neon.api_key,
            &self.database_neon_api_key,
        );
        set_if_some(
            &mut config.database.neon.project_id,
            &self.database_neon_project_id,
        );
        set_if_some(
            &mut config.database.neon.parent_branch_id,
            &self.database_neon_parent_branch_id,
        );
        set_copy_if_some(
            &mut config.database.neon.max_checkpoints,
            self.database_neon_max_checkpoints,
        );
        set_copy_if_some(
            &mut config.database.neon.checkpoint_ttl_hours,
            self.database_neon_checkpoint_ttl_hours,
        );
        set_copy_if_some(&mut config.database.neon.pooled, self.database_neon_pooled);
        set_copy_if_some(
            &mut config.database.neon.suspend_timeout_seconds,
            self.database_neon_suspend_timeout_seconds,
        );
    }
}

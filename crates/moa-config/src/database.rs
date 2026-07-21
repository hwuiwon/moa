//! Database connection and Neon checkpoint configuration.

use serde::{Deserialize, Serialize};

/// Built-in development database URL, used when no URL is configured.
///
/// It embeds the local compose dev password, so a deployment left on this value
/// is running against development credentials; config validation warns when it is
/// in effect.
pub const BUILTIN_DEV_DATABASE_URL: &str = "postgres://moa_owner:dev@localhost:10040/moa";

/// Session database configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DatabaseConfig {
    /// Runtime Postgres connection URL.
    pub url: String,
    /// Optional direct/admin database URL for migrations and other session-sensitive flows.
    pub admin_url: Option<String>,
    /// Optional already-provisioned Postgres schema for runtime queries.
    ///
    /// When set, session-store construction binds to this schema and does not
    /// run migrations. Automatic full-database migration is only used when this
    /// value is `None`.
    pub schema: Option<String>,
    /// Maximum pool size for the shared Postgres client.
    pub max_connections: u32,
    /// Maximum pool size reserved for process-owned background workers.
    ///
    /// This is a separate pool from foreground Restate handlers so maintenance,
    /// export, and outbox work cannot consume the foreground connection budget.
    pub background_max_connections: u32,
    /// Pool acquire timeout, in seconds.
    ///
    /// Applied as the sqlx pool `acquire_timeout` (see `runtime::database`): the
    /// maximum time a caller waits to check out a pooled connection, which also
    /// bounds establishing a new connection when the pool must grow to serve the
    /// request. It is not limited to the initial connection handshake.
    pub connect_timeout_seconds: u64,
    /// Optional Neon branching configuration for ephemeral checkpoints.
    pub neon: DatabaseNeonConfig,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: BUILTIN_DEV_DATABASE_URL.to_string(),
            admin_url: None,
            schema: None,
            max_connections: 20,
            background_max_connections: 2,
            connect_timeout_seconds: 10,
            neon: DatabaseNeonConfig::default(),
        }
    }
}

impl DatabaseConfig {
    /// Returns whether the runtime URL is the built-in development default,
    /// which embeds dev credentials and should not be used in production.
    pub fn uses_builtin_dev_url(&self) -> bool {
        self.url == BUILTIN_DEV_DATABASE_URL
    }

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

#[cfg(test)]
mod tests {
    use super::{BUILTIN_DEV_DATABASE_URL, DatabaseConfig};

    #[test]
    fn uses_builtin_dev_url_detects_the_default_dev_credentials() {
        // Pins: the condition that drives the dev-credential startup warning — the
        // built-in default URL is recognized, and any configured URL is not.
        let default = DatabaseConfig::default();
        assert_eq!(default.url, BUILTIN_DEV_DATABASE_URL);
        assert!(default.uses_builtin_dev_url());

        let configured = DatabaseConfig {
            url: "postgres://user:secret@prod-db:5432/moa".to_string(),
            ..DatabaseConfig::default()
        };
        assert!(!configured.uses_builtin_dev_url());
    }
}

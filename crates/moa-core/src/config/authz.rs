//! `[authz]` and `[authz.openfga]` configuration sections.
//!
//! Environment variable equivalents follow the flat `MOA_SECTION_KEY` convention:
//! `MOA_AUTHZ_ENGINE`, `MOA_AUTHZ_OPENFGA_URL`,
//! `MOA_AUTHZ_OPENFGA_PRESHARED_KEY`, `MOA_AUTHZ_OPENFGA_STORE_ID`,
//! `MOA_AUTHZ_OPENFGA_MODEL_ID`, and `MOA_AUTHZ_OPENFGA_TIMEOUT_MS`.

use serde::{Deserialize, Serialize};

/// Authorization subsystem configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthzConfig {
    /// Authorization engine selection.
    #[serde(default)]
    pub engine: AuthzEngine,
    /// OpenFGA settings required when `engine = "openfga"`.
    #[serde(default)]
    pub openfga: Option<OpenFgaConfig>,
}

impl Default for AuthzConfig {
    fn default() -> Self {
        Self {
            engine: AuthzEngine::Openfga,
            openfga: None,
        }
    }
}

/// Supported authorization engines.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthzEngine {
    /// Self-hosted OpenFGA, Postgres-backed.
    #[default]
    Openfga,
    /// Future managed Auth0 FGA swap-in.
    Auth0Fga,
}

impl AuthzEngine {
    /// Return the serialized configuration value for this engine.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Openfga => "openfga",
            Self::Auth0Fga => "auth0_fga",
        }
    }
}

/// OpenFGA connection settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenFgaConfig {
    /// OpenFGA HTTP API base URL.
    pub url: String,
    /// Preshared key configured in OpenFGA.
    pub preshared_key: String,
    /// OpenFGA store ID.
    pub store_id: String,
    /// OpenFGA authorization model ID.
    pub model_id: String,
    /// Per-request HTTP timeout in milliseconds.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    5000
}

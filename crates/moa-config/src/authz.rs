//! `[authz]` and `[authz.openfga]` configuration sections.
//!
//! Environment variable equivalents follow the flat `MOA_SECTION_KEY` convention:
//! `MOA_AUTHZ_ENGINE`, `MOA_AUTHZ_OPENFGA_URL`,
//! `MOA_AUTHZ_OPENFGA_PRESHARED_KEY`, `MOA_AUTHZ_OPENFGA_STORE_ID`,
//! `MOA_AUTHZ_OPENFGA_MODEL_ID`, and `MOA_AUTHZ_OPENFGA_TIMEOUT_MS`.

use serde::{Deserialize, Serialize};

/// Default per-request OpenFGA timeout.
///
/// Authorization checks are on a fail-closed hot path evaluated for nearly every
/// request, so a degraded OpenFGA must not stall each request for long. Kept
/// short (2s) to fail fast rather than amplify an OpenFGA slowdown into
/// request-wide latency.
const OPENFGA_DEFAULT_TIMEOUT_MS: u64 = 2000;

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
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
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
        self.into()
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
    /// Logical authorization-model version expected at the configured model ID.
    pub model_version: u32,
    /// Per-request HTTP timeout in milliseconds. Kept short because authz is a
    /// fail-closed hot path; a slow OpenFGA should fail fast, not stall requests.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    OPENFGA_DEFAULT_TIMEOUT_MS
}

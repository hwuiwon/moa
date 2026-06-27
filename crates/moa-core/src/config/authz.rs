//! `[authz]` and `[authz.openfga]` configuration sections.
//!
//! Environment variable equivalents follow the flat `MOA_SECTION_KEY` convention:
//! `MOA_AUTHZ_ENGINE`, `MOA_AUTHZ_OPENFGA_URL`,
//! `MOA_AUTHZ_OPENFGA_PRESHARED_KEY`, `MOA_AUTHZ_OPENFGA_STORE_ID`,
//! `MOA_AUTHZ_OPENFGA_MODEL_ID`, and `MOA_AUTHZ_OPENFGA_TIMEOUT_MS`.

use serde::{Deserialize, Serialize};

const OPENFGA_DEFAULT_TIMEOUT_MS: u64 = 5000;

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
    /// Per-request HTTP timeout in milliseconds.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    OPENFGA_DEFAULT_TIMEOUT_MS
}

impl super::MoaEnvOverlay {
    /// Applies authorization and OpenFGA environment overrides.
    pub(in crate::config) fn apply_authz_overlay(
        &self,
        config: &mut super::MoaConfig,
    ) -> crate::Result<()> {
        use super::env_overlay::{any_present, require_non_empty, set_copy_if_some, set_if_some};

        set_copy_if_some(&mut config.authz.engine, self.authz_engine);
        if !any_present(&[
            self.authz_openfga_url.is_some(),
            self.authz_openfga_preshared_key.is_some(),
            self.authz_openfga_store_id.is_some(),
            self.authz_openfga_model_id.is_some(),
            self.authz_openfga_timeout_ms.is_some(),
        ]) {
            return Ok(());
        }

        let mut openfga = config
            .authz
            .openfga
            .clone()
            .unwrap_or_else(|| OpenFgaConfig {
                url: String::new(),
                preshared_key: String::new(),
                store_id: String::new(),
                model_id: String::new(),
                timeout_ms: OPENFGA_DEFAULT_TIMEOUT_MS,
            });
        set_if_some(&mut openfga.url, &self.authz_openfga_url);
        set_if_some(
            &mut openfga.preshared_key,
            &self.authz_openfga_preshared_key,
        );
        set_if_some(&mut openfga.store_id, &self.authz_openfga_store_id);
        set_if_some(&mut openfga.model_id, &self.authz_openfga_model_id);
        set_copy_if_some(&mut openfga.timeout_ms, self.authz_openfga_timeout_ms);
        require_non_empty("MOA_AUTHZ_OPENFGA_URL", &openfga.url)?;
        require_non_empty("MOA_AUTHZ_OPENFGA_PRESHARED_KEY", &openfga.preshared_key)?;
        require_non_empty("MOA_AUTHZ_OPENFGA_STORE_ID", &openfga.store_id)?;
        require_non_empty("MOA_AUTHZ_OPENFGA_MODEL_ID", &openfga.model_id)?;
        config.authz.openfga = Some(openfga);
        Ok(())
    }
}

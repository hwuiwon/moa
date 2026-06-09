//! Runtime configuration helpers for the Restate orchestrator binary.

use std::path::PathBuf;

use anyhow::{Context as AnyhowContext, Result, bail};
use moa_core::MoaConfig;

/// Loads the orchestrator's shared MOA runtime configuration from environment variables.
pub fn load_moa_config_from_env() -> Result<MoaConfig> {
    let mut config = MoaConfig::load_from_env().context("load MOA config from environment")?;
    config.observability.service_name = "moa-orchestrator".to_string();
    config
        .observability
        .release
        .get_or_insert_with(|| env!("CARGO_PKG_VERSION").to_string());
    Ok(config)
}

/// Returns whether OpenFGA-backed authz should be skipped for local runs.
#[must_use]
pub fn skip_fga_from_env() -> bool {
    env_flag_from_reader("MOA_SKIP_FGA", false, |key| std::env::var(key).ok())
}

/// Resolves the Restate admin URL from the shared MOA config.
pub fn restate_admin_url(config: &MoaConfig) -> Result<String> {
    config
        .orchestrator
        .restate_admin_url
        .clone()
        .context("orchestrator.restate_admin_url config missing")
}

/// Resolves the Restate ingress URL from the shared MOA config.
pub fn restate_ingress_url(config: &MoaConfig) -> Result<String> {
    config
        .orchestrator
        .restate_ingress_url
        .clone()
        .or_else(|| config.orchestrator.endpoint.clone())
        .context("orchestrator.restate_ingress_url config missing")
}

fn env_flag_from_reader(
    key: &str,
    default: bool,
    mut read_var: impl FnMut(&str) -> Option<String>,
) -> bool {
    read_var(key)
        .and_then(|value: String| match value.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        })
        .unwrap_or(default)
}

/// Provider override mode selected by `MOA_PROVIDERS_OVERRIDE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvidersOverride {
    /// Use providers configured from normal credentials.
    None,
    /// Use a scripted provider fixture loaded from disk.
    Scripted {
        /// JSON fixture path.
        path: PathBuf,
    },
    /// Use a deterministic built-in mock provider.
    Mock {
        /// Seed recorded for reproducible diagnostics.
        seed: u64,
    },
}

impl ProvidersOverride {
    /// Reads the provider override mode from `MOA_PROVIDERS_OVERRIDE`.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_raw(std::env::var("MOA_PROVIDERS_OVERRIDE").ok().as_deref())
    }

    /// Returns true when an override is active.
    #[must_use]
    pub fn is_active(&self) -> bool {
        !matches!(self, Self::None)
    }

    /// Fails if a provider override is configured in a production environment.
    pub fn ensure_allowed(&self, config: &MoaConfig) -> Result<()> {
        if self.is_active() && production_environment(config) {
            bail!("MOA_PROVIDERS_OVERRIDE is not allowed when environment=prod");
        }
        Ok(())
    }

    fn from_raw(raw: Option<&str>) -> Self {
        let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
            return Self::None;
        };

        match raw.split_once(':') {
            Some(("scripted", path)) if !path.trim().is_empty() => Self::Scripted {
                path: PathBuf::from(path.trim()),
            },
            Some(("mock", seed)) => Self::Mock {
                seed: seed.trim().parse().unwrap_or(0),
            },
            _ => {
                tracing::warn!(value = %raw, "MOA_PROVIDERS_OVERRIDE not parseable; ignoring");
                Self::None
            }
        }
    }
}

fn production_environment(config: &MoaConfig) -> bool {
    is_prod_value(config.observability.environment.as_deref())
}

fn is_prod_value(value: Option<&str>) -> bool {
    value
        .map(str::trim)
        .map(|value| value.eq_ignore_ascii_case("prod") || value.eq_ignore_ascii_case("production"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use moa_core::MoaConfig;

    use super::{ProvidersOverride, env_flag_from_reader, restate_admin_url, restate_ingress_url};

    #[test]
    fn restate_urls_resolve_from_moa_config() {
        // Pins: orchestrator boot reads Restate URLs from the shared MoaConfig.
        let mut config = MoaConfig::default();
        config.orchestrator.restate_admin_url = Some("http://restate:9070".to_string());
        config.orchestrator.restate_ingress_url = Some("http://restate:8080".to_string());

        assert_eq!(
            restate_admin_url(&config).expect("admin url"),
            "http://restate:9070"
        );
        assert_eq!(
            restate_ingress_url(&config).expect("ingress url"),
            "http://restate:8080"
        );
    }

    #[test]
    fn providers_override_parses_scripted_fixture_path() {
        let override_mode =
            ProvidersOverride::from_raw(Some("scripted:/tmp/moa-loadtest-script.json"));

        assert_eq!(
            override_mode,
            ProvidersOverride::Scripted {
                path: "/tmp/moa-loadtest-script.json".into()
            }
        );
    }

    #[test]
    fn providers_override_blocks_prod_environment() {
        let mut config = MoaConfig::default();
        config.observability.environment = Some("prod".to_string());

        let error = ProvidersOverride::Mock { seed: 7 }
            .ensure_allowed(&config)
            .expect_err("provider overrides must be blocked in prod");

        assert_eq!(
            error.to_string(),
            "MOA_PROVIDERS_OVERRIDE is not allowed when environment=prod"
        );
    }

    #[test]
    fn env_flag_understands_common_truthy_and_falsey_values() {
        // Pins: local process-only flags keep predictable bool parsing after config collapse.
        assert!(env_flag_from_reader(
            "MOA_SKIP_FGA",
            false,
            |key| match key {
                "MOA_SKIP_FGA" => Some("yes".to_string()),
                _ => None,
            }
        ));
        assert!(!env_flag_from_reader(
            "MOA_SKIP_FGA",
            true,
            |key| match key {
                "MOA_SKIP_FGA" => Some("off".to_string()),
                _ => None,
            }
        ));
        assert!(env_flag_from_reader("MOA_SKIP_FGA", true, |_| None));
    }
}

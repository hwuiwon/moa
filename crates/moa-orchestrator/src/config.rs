//! Environment-backed configuration for the Restate orchestrator binary.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use moa_core::{MoaConfig, OpenFgaConfig, OtlpProtocol};
use serde::Deserialize;

/// Runtime configuration for the Restate-backed orchestrator.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct OrchestratorConfig {
    /// Base URL for the Restate admin API.
    pub restate_admin_url: String,
    /// Postgres connection string for future orchestrator services.
    pub postgres_url: String,
    /// Optional LLM gateway URL for future service calls.
    pub llm_gateway_url: Option<String>,
    /// Optional override for the local sandbox root used by tool hands.
    pub sandbox_dir: Option<String>,
    /// Optional override for the file-backed memory root.
    pub memory_dir: Option<String>,
    /// Whether Docker-backed local hands are enabled.
    pub docker_enabled: bool,
    /// Optional OTLP exporter endpoint used for tracing.
    pub otlp_endpoint: Option<String>,
    /// OTLP transport protocol.
    pub otlp_protocol: OtlpProtocol,
    /// Additional OTLP headers for exporter auth and routing.
    pub otlp_headers: HashMap<String, String>,
    /// Optional deployment environment resource attribute.
    pub deploy_env: Option<String>,
    /// Whether the Prometheus scrape endpoint should be enabled.
    pub metrics_enabled: bool,
    /// Optional override for the Prometheus listener address.
    pub metrics_listen: Option<String>,
    /// Whether OpenFGA-backed authz is disabled for local non-auth work.
    pub skip_fga: bool,
    /// OpenFGA config for the authz outbox poller.
    pub authz_openfga: Option<OpenFgaConfig>,
}

impl OrchestratorConfig {
    /// Loads the orchestrator configuration from process environment variables.
    pub fn from_env() -> Result<Self> {
        Self::from_reader(|key| std::env::var(key).ok())
    }

    /// Converts the environment-backed settings into a `MoaConfig` used by shared subsystems.
    #[must_use]
    pub fn to_moa_config(&self) -> MoaConfig {
        let mut config = MoaConfig::default();
        config.database.url = self.postgres_url.clone();
        config.local.docker_enabled = self.docker_enabled;
        config.observability.enabled = self.otlp_endpoint.is_some();
        config.observability.service_name = "moa-orchestrator".to_string();
        config.observability.otlp_endpoint = self.otlp_endpoint.clone();
        config.observability.otlp_protocol = self.otlp_protocol;
        config.observability.otlp_headers = self.otlp_headers.clone();
        config.observability.environment = self.deploy_env.clone();
        config.observability.release = Some(env!("CARGO_PKG_VERSION").to_string());
        config.metrics.enabled = self.metrics_enabled;
        if let Some(listen) = &self.metrics_listen {
            config.metrics.listen = listen.clone();
        }
        if let Some(sandbox_dir) = &self.sandbox_dir {
            config.local.sandbox_dir = sandbox_dir.clone();
        }
        if let Some(memory_dir) = &self.memory_dir {
            config.local.memory_dir = memory_dir.clone();
        }
        config.authz.openfga = self.authz_openfga.clone();
        config
    }

    fn from_reader(mut read_var: impl FnMut(&str) -> Option<String>) -> Result<Self> {
        let postgres_url =
            read_var("POSTGRES_URL").ok_or_else(|| anyhow!("POSTGRES_URL required"))?;
        let skip_fga = read_var("MOA_SKIP_FGA")
            .map(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "True"))
            .unwrap_or(false);
        let openfga_url = read_first(
            &mut read_var,
            &["MOA__AUTHZ__OPENFGA__URL", "MOA_OPENFGA_URL"],
        );
        let openfga_preshared_key = read_first(
            &mut read_var,
            &[
                "MOA__AUTHZ__OPENFGA__PRESHARED_KEY",
                "MOA_OPENFGA_PRESHARED_KEY",
            ],
        );
        let openfga_store_id = read_first(
            &mut read_var,
            &["MOA__AUTHZ__OPENFGA__STORE_ID", "MOA_OPENFGA_STORE_ID"],
        );
        let openfga_model_id = read_first(
            &mut read_var,
            &["MOA__AUTHZ__OPENFGA__MODEL_ID", "MOA_OPENFGA_MODEL_ID"],
        );
        let openfga_timeout_ms = read_first(&mut read_var, &["MOA__AUTHZ__OPENFGA__TIMEOUT_MS"])
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(5000);
        let authz_openfga = match (
            openfga_url,
            openfga_preshared_key,
            openfga_store_id,
            openfga_model_id,
        ) {
            (Some(url), Some(preshared_key), Some(store_id), Some(model_id)) => {
                Some(OpenFgaConfig {
                    url,
                    preshared_key,
                    store_id,
                    model_id,
                    timeout_ms: openfga_timeout_ms,
                })
            }
            (None, None, None, None) => None,
            _ => {
                return Err(anyhow!(
                    "OpenFGA authz config must include url, preshared key, store id, and model id"
                ));
            }
        };

        Ok(Self {
            restate_admin_url: read_var("RESTATE_ADMIN_URL")
                .unwrap_or_else(|| "http://localhost:9070".to_string()),
            postgres_url,
            llm_gateway_url: read_var("LLM_GATEWAY_URL"),
            sandbox_dir: read_var("MOA_SANDBOX_DIR"),
            memory_dir: read_var("MOA_MEMORY_DIR"),
            docker_enabled: read_var("MOA_DOCKER_ENABLED")
                .and_then(|value| value.parse::<bool>().ok())
                .unwrap_or_else(|| MoaConfig::default().local.docker_enabled),
            otlp_endpoint: read_var("OTEL_EXPORTER_OTLP_ENDPOINT"),
            otlp_protocol: parse_otlp_protocol(read_var("OTEL_EXPORTER_OTLP_PROTOCOL")),
            otlp_headers: parse_otlp_headers(read_var("OTEL_EXPORTER_OTLP_HEADERS")),
            deploy_env: read_var("DEPLOY_ENV"),
            metrics_enabled: read_var("MOA_METRICS_ENABLED")
                .and_then(|value| value.parse::<bool>().ok())
                .unwrap_or_else(|| MoaConfig::default().metrics.enabled),
            metrics_listen: read_var("MOA_METRICS_LISTEN"),
            skip_fga,
            authz_openfga,
        })
    }
}

fn read_first(read_var: &mut impl FnMut(&str) -> Option<String>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| read_var(key))
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
    if is_prod_value(config.observability.environment.as_deref()) {
        return true;
    }

    ["MOA__ENVIRONMENT", "MOA_ENVIRONMENT", "DEPLOY_ENV"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .any(|value| is_prod_value(Some(value.as_str())))
}

fn is_prod_value(value: Option<&str>) -> bool {
    value
        .map(str::trim)
        .map(|value| value.eq_ignore_ascii_case("prod") || value.eq_ignore_ascii_case("production"))
        .unwrap_or(false)
}

fn parse_otlp_protocol(raw: Option<String>) -> OtlpProtocol {
    match raw.as_deref().map(str::trim) {
        Some("http") | Some("http/protobuf") => OtlpProtocol::Http,
        _ => OtlpProtocol::Grpc,
    }
}

fn parse_otlp_headers(raw: Option<String>) -> std::collections::HashMap<String, String> {
    raw.unwrap_or_default()
        .split(',')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            let key = key.trim();
            let value = value.trim();
            if key.is_empty() || value.is_empty() {
                return None;
            }
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use moa_core::MoaConfig;

    use super::{OrchestratorConfig, ProvidersOverride};

    #[test]
    fn from_reader_uses_defaults_and_optional_values() {
        let config = OrchestratorConfig::from_reader(|key| match key {
            "POSTGRES_URL" => Some("postgres://example".to_string()),
            _ => None,
        })
        .expect("config should load");

        assert_eq!(config.restate_admin_url, "http://localhost:9070");
        assert_eq!(config.postgres_url, "postgres://example");
        assert_eq!(config.llm_gateway_url, None);
        assert_eq!(config.sandbox_dir, None);
        assert_eq!(config.memory_dir, None);
        assert!(config.docker_enabled);
    }

    #[test]
    fn from_reader_requires_postgres_url() {
        let error = OrchestratorConfig::from_reader(|_| None)
            .expect_err("missing POSTGRES_URL should error");

        assert_eq!(error.to_string(), "POSTGRES_URL required");
    }

    #[test]
    fn to_moa_config_applies_local_overrides() {
        let config = OrchestratorConfig::from_reader(|key| match key {
            "POSTGRES_URL" => Some("postgres://example".to_string()),
            "MOA_SANDBOX_DIR" => Some("/tmp/moa-sandbox".to_string()),
            "MOA_MEMORY_DIR" => Some("/tmp/moa-memory".to_string()),
            "MOA_DOCKER_ENABLED" => Some("false".to_string()),
            _ => None,
        })
        .expect("config should load");

        let moa_config = config.to_moa_config();
        assert_eq!(moa_config.database.url, "postgres://example");
        assert_eq!(moa_config.local.sandbox_dir, "/tmp/moa-sandbox");
        assert_eq!(moa_config.local.memory_dir, "/tmp/moa-memory");
        assert!(!moa_config.local.docker_enabled);
    }

    #[test]
    fn authz_openfga_env_maps_into_moa_config() {
        let config = OrchestratorConfig::from_reader(|key| match key {
            "POSTGRES_URL" => Some("postgres://example".to_string()),
            "MOA__AUTHZ__OPENFGA__URL" => Some("http://openfga:8080".to_string()),
            "MOA__AUTHZ__OPENFGA__PRESHARED_KEY" => Some("dev-key".to_string()),
            "MOA__AUTHZ__OPENFGA__STORE_ID" => Some("store-1".to_string()),
            "MOA__AUTHZ__OPENFGA__MODEL_ID" => Some("model-1".to_string()),
            "MOA__AUTHZ__OPENFGA__TIMEOUT_MS" => Some("2500".to_string()),
            _ => None,
        })
        .expect("config should load");

        assert!(!config.skip_fga);
        let moa_config = config.to_moa_config();
        let openfga = moa_config
            .authz
            .openfga
            .expect("openfga config should map into MoaConfig");
        assert_eq!(openfga.url, "http://openfga:8080");
        assert_eq!(openfga.preshared_key, "dev-key");
        assert_eq!(openfga.store_id, "store-1");
        assert_eq!(openfga.model_id, "model-1");
        assert_eq!(openfga.timeout_ms, 2500);
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
}

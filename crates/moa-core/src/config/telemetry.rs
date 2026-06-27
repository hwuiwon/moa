//! Observability and metrics configuration.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::lineage::LineageConfig;

/// Observability configuration.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, strum::IntoStaticStr,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum OtlpProtocol {
    /// Export OTLP spans over gRPC.
    #[default]
    Grpc,
    /// Export OTLP spans over HTTP protobuf.
    Http,
}

impl OtlpProtocol {
    /// Returns the serialized config string for this protocol.
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Observability configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ObservabilityConfig {
    /// Whether OTLP export is enabled.
    pub enabled: bool,
    /// Logical service name for traces.
    pub service_name: String,
    /// Optional OTLP endpoint override.
    pub otlp_endpoint: Option<String>,
    /// OTLP transport protocol.
    pub otlp_protocol: OtlpProtocol,
    /// Additional OTLP headers for exporter auth and routing.
    pub otlp_headers: HashMap<String, String>,
    /// Deployment environment resource attribute.
    pub environment: Option<String>,
    /// Application release or version resource attribute.
    pub release: Option<String>,
    /// Trace sampling ratio from 0.0 to 1.0.
    pub sample_rate: f64,
    /// Durable lineage capture settings.
    pub lineage: LineageConfig,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            service_name: "moa".to_string(),
            otlp_endpoint: None,
            otlp_protocol: OtlpProtocol::Grpc,
            otlp_headers: HashMap::new(),
            environment: None,
            release: None,
            sample_rate: 1.0,
            lineage: LineageConfig::default(),
        }
    }
}

/// Prometheus metrics export configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MetricsConfig {
    /// Whether the Prometheus scrape endpoint should be exposed.
    pub enabled: bool,
    /// Listener address for the Prometheus scrape endpoint.
    pub listen: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: "0.0.0.0:9090".to_string(),
        }
    }
}

impl super::MoaEnvOverlay {
    /// Applies observability environment overrides.
    pub(in crate::config) fn apply_observability_overlay(&self, config: &mut super::MoaConfig) {
        use super::env_overlay::{set_copy_if_some, set_if_some, set_option_if_some};

        set_copy_if_some(
            &mut config.observability.enabled,
            self.observability_enabled,
        );
        set_if_some(
            &mut config.observability.service_name,
            &self.observability_service_name,
        );
        set_option_if_some(
            &mut config.observability.otlp_endpoint,
            &self.observability_otlp_endpoint,
        );
        set_copy_if_some(
            &mut config.observability.otlp_protocol,
            self.observability_otlp_protocol,
        );
        if let Some(headers) = &self.observability_otlp_headers {
            config.observability.otlp_headers = headers.clone();
        }
        set_option_if_some(
            &mut config.observability.environment,
            &self.observability_environment,
        );
        set_option_if_some(
            &mut config.observability.release,
            &self.observability_release,
        );
        set_copy_if_some(
            &mut config.observability.sample_rate,
            self.observability_sample_rate,
        );
        set_copy_if_some(
            &mut config.observability.lineage.enabled,
            self.observability_lineage_enabled,
        );
        set_copy_if_some(
            &mut config.observability.lineage.channel_capacity,
            self.observability_lineage_channel_capacity,
        );
        set_copy_if_some(
            &mut config.observability.lineage.batch_size,
            self.observability_lineage_batch_size,
        );
        set_copy_if_some(
            &mut config.observability.lineage.batch_max_age_secs,
            self.observability_lineage_batch_max_age_secs,
        );
        set_if_some(
            &mut config.observability.lineage.journal_path,
            &self.observability_lineage_journal_path,
        );
        set_copy_if_some(
            &mut config.observability.lineage.sample_pgvector_explain,
            self.observability_lineage_sample_pgvector_explain,
        );
    }

    /// Applies metrics endpoint environment overrides.
    pub(in crate::config) fn apply_metrics_overlay(&self, config: &mut super::MoaConfig) {
        use super::env_overlay::{set_copy_if_some, set_if_some};

        set_copy_if_some(&mut config.metrics.enabled, self.metrics_enabled);
        set_if_some(&mut config.metrics.listen, &self.metrics_listen);
    }
}

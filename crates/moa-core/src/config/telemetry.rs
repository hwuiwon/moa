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
    ///
    /// Defaults to `0.01` (1%) so a production fleet does not export a full-fidelity span per turn
    /// by default. Override per environment (for example `1.0` in local development) as needed.
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
            sample_rate: 0.01,
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

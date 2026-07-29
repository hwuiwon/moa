//! Observability and metrics configuration.

use std::collections::HashMap;

use moa_core::error::{MoaError, Result};
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

/// Where runtime metrics are exported.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, strum::IntoStaticStr,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum MetricsExporter {
    /// Push runtime metrics to the OTLP collector. The production default.
    ///
    /// Production runs behind a load balancer with non-sticky routing and an
    /// autoscaled replica count, so a scrape endpoint on a pod would be scraped
    /// through the service and land on an arbitrary replica each interval. The
    /// resulting series is a blend of unrelated processes: counters go
    /// backwards, gauges flip between replicas, and no query over it means
    /// anything. Pushing is the only model that survives that topology.
    #[default]
    Otlp,
    /// Serve a Prometheus scrape endpoint. Development and single-process only.
    Prometheus,
    /// Export nothing. `metrics!` calls become no-ops.
    Disabled,
}

impl MetricsExporter {
    /// Returns the serialized config string for this exporter.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Runtime metrics export configuration.
///
/// Replaces the previous `enabled`/`listen` pair outright. That pair could not
/// express the production answer at all: "enabled" meant "serve a scrape
/// endpoint", so the only way to stop advertising an endpoint nothing could
/// usefully scrape was to turn metrics off entirely. Unknown fields are refused
/// so a config still carrying `enabled` or `listen` fails at load rather than
/// silently exporting nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// Selected exporter.
    pub exporter: MetricsExporter,
    /// Listener address for the Prometheus scrape endpoint.
    ///
    /// Required by, and only meaningful for, [`MetricsExporter::Prometheus`].
    /// There is deliberately no default: a default would let the dev-only
    /// exporter come up on an address nobody chose.
    pub prometheus_listen: Option<String>,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            exporter: MetricsExporter::Otlp,
            prometheus_listen: None,
        }
    }
}

impl MetricsConfig {
    /// Refuses a metrics configuration that cannot be served as written.
    pub fn validate(&self) -> Result<()> {
        match self.exporter {
            MetricsExporter::Prometheus if self.prometheus_listen.is_none() => {
                Err(MoaError::ConfigError(
                    "metrics.exporter = \"prometheus\" requires metrics.prometheus_listen \
                     (MOA_METRICS_PROMETHEUS_LISTEN); the scrape exporter has no default \
                     address because it is a development-only mode"
                        .to_string(),
                ))
            }
            MetricsExporter::Otlp | MetricsExporter::Disabled
                if self.prometheus_listen.is_some() =>
            {
                Err(MoaError::ConfigError(format!(
                    "metrics.prometheus_listen is set but metrics.exporter is \"{}\"; a listen \
                     address that is never bound reads as a scrape endpoint that exists",
                    self.exporter.as_str()
                )))
            }
            _ => Ok(()),
        }
    }
}

/// One of the OTLP signals MOA exports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtlpSignal {
    /// Distributed traces.
    Traces,
    /// Runtime metrics.
    Metrics,
}

impl OtlpSignal {
    /// Returns the OTLP/HTTP path for this signal.
    fn path(self) -> &'static str {
        match self {
            Self::Traces => "/v1/traces",
            Self::Metrics => "/v1/metrics",
        }
    }
}

/// Resolves the exporter endpoint for one signal over one transport.
///
/// This is the ONLY way an OTLP endpoint string is produced, and both signals go
/// through it, because the two transports need opposite things from the same
/// configured value and getting that wrong is silent in both directions:
///
///   * On **HTTP**, `opentelemetry-otlp` uses a programmatically supplied
///     endpoint verbatim and appends no signal path (only its environment
///     variable branch calls `build_endpoint_uri`). A base URL passed straight
///     through therefore POSTs every payload to the collector root, which 404s
///     with nothing in-process reporting a problem.
///   * On **gRPC**, tonic appends the service method itself. Appending a signal
///     path here would produce `/v1/metrics/opentelemetry.proto...` and fail.
///
/// `MOA_OTLP_ENDPOINT` names the collector, not one of its signal paths, so a
/// value that already ends in a signal path is refused: it names one signal's
/// endpoint and leaves nothing to derive the other's from.
pub fn otlp_signal_endpoint(
    base: &str,
    protocol: OtlpProtocol,
    signal: OtlpSignal,
) -> Result<String> {
    let base = validated_otlp_base(base)?;
    Ok(match protocol {
        OtlpProtocol::Http => format!("{base}{}", signal.path()),
        OtlpProtocol::Grpc => base.to_string(),
    })
}

/// Signal paths a collector base URL must not already carry.
const OTLP_SIGNAL_PATHS: [&str; 3] = ["/v1/traces", "/v1/metrics", "/v1/logs"];

fn validated_otlp_base(base: &str) -> Result<&str> {
    let trimmed = base.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(MoaError::ConfigError(
            "observability.otlp_endpoint must not be empty".to_string(),
        ));
    }
    if let Some(path) = OTLP_SIGNAL_PATHS
        .into_iter()
        .find(|path| trimmed.ends_with(path))
    {
        return Err(MoaError::ConfigError(format!(
            "observability.otlp_endpoint `{base}` names the `{path}` signal endpoint; it must be \
             the collector base URL so traces and metrics are derived from the same collector \
             (drop the `{path}` suffix)"
        )));
    }
    Ok(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prometheus_exporter_requires_an_explicit_listen_address() {
        // Pins: the dev-only scrape exporter cannot come up on an address nobody
        // chose. A default here would put a scrape endpoint on every process that
        // merely asked for Prometheus.
        let error = MetricsConfig {
            exporter: MetricsExporter::Prometheus,
            prometheus_listen: None,
        }
        .validate()
        .expect_err("prometheus without a listen address must be refused");
        assert!(
            error.to_string().contains("prometheus_listen"),
            "the refusal must name the missing key, got: {error}"
        );

        MetricsConfig {
            exporter: MetricsExporter::Prometheus,
            prometheus_listen: Some("127.0.0.1:9090".to_string()),
        }
        .validate()
        .expect("an explicit listen address is the supported development mode");
    }

    #[test]
    fn a_listen_address_without_the_prometheus_exporter_is_refused() {
        // Pins: a configured-but-unbound listen address is worse than none. It
        // reads to an operator, a manifest, and a network policy as a scrape
        // endpoint that exists, which is the exact fiction this task removes.
        for exporter in [MetricsExporter::Otlp, MetricsExporter::Disabled] {
            let error = MetricsConfig {
                exporter,
                prometheus_listen: Some("0.0.0.0:9090".to_string()),
            }
            .validate()
            .expect_err("a listen address is only meaningful for the prometheus exporter");
            assert!(
                error.to_string().contains(exporter.as_str()),
                "the refusal must name the selected exporter, got: {error}"
            );
        }
    }

    #[test]
    fn the_default_exporter_is_otlp() {
        // Pins: production gets push export without opting in. The previous
        // default served nothing, so a fleet that never set the key exported no
        // runtime metrics at all.
        assert_eq!(MetricsConfig::default().exporter, MetricsExporter::Otlp);
        MetricsConfig::default()
            .validate()
            .expect("the default configuration must be valid as written");
    }

    #[test]
    fn old_metrics_keys_fail_to_load_instead_of_being_ignored() {
        // Pins: a config still carrying the removed keys fails loudly. Serde's
        // default behaviour is to ignore unknown fields, which would leave an
        // operator's `enabled = true` silently doing nothing.
        // Format-independent: `deny_unknown_fields` is a serde property, and the
        // TOML loader deserializes through the same derive.
        for legacy in [
            serde_json::json!({ "enabled": true }),
            serde_json::json!({ "listen": "0.0.0.0:9090" }),
        ] {
            let error = serde_json::from_value::<MetricsConfig>(legacy.clone())
                .expect_err("a removed metrics key must fail to load, not be ignored");
            assert!(
                error.to_string().contains("unknown field"),
                "loading `{legacy}` must report the unknown field, got: {error}"
            );
        }
    }

    #[test]
    fn http_endpoints_carry_the_signal_path_and_grpc_endpoints_do_not() {
        // Pins the transport asymmetry, which is the whole reason this function
        // exists. On HTTP the SDK uses a programmatic endpoint verbatim, so the
        // path must be here or every payload 404s at the collector root. On gRPC
        // tonic appends the service method, so the same path would corrupt the
        // URI. Both directions are silent failures, and both signals resolve
        // through this one function so they cannot drift apart.
        for signal in [OtlpSignal::Traces, OtlpSignal::Metrics] {
            let http = otlp_signal_endpoint("http://alloy:4318", OtlpProtocol::Http, signal)
                .expect("base URL should be accepted");
            assert!(
                http.ends_with(signal.path()),
                "HTTP must resolve the signal path itself, got {http}"
            );

            let grpc = otlp_signal_endpoint("http://alloy:4317/", OtlpProtocol::Grpc, signal)
                .expect("base URL should be accepted");
            assert_eq!(
                grpc, "http://alloy:4317",
                "gRPC must keep the bare collector endpoint; tonic appends the method"
            );
        }
    }

    #[test]
    fn both_signals_resolve_against_the_same_collector() {
        // Pins: one configured value cannot send traces and metrics to different
        // collectors, whatever the transport.
        for protocol in [OtlpProtocol::Http, OtlpProtocol::Grpc] {
            let traces = otlp_signal_endpoint("http://alloy:4318", protocol, OtlpSignal::Traces)
                .expect("base URL should be accepted");
            let metrics = otlp_signal_endpoint("http://alloy:4318", protocol, OtlpSignal::Metrics)
                .expect("base URL should be accepted");
            assert!(
                traces.starts_with("http://alloy:4318") && metrics.starts_with("http://alloy:4318"),
                "both signals must target the configured collector, got {traces} and {metrics}"
            );
        }
    }

    #[test]
    fn a_signal_specific_otlp_endpoint_is_refused() {
        // Pins: the old value shape fails loudly. A base of
        // `http://alloy:4318/v1/traces` would otherwise derive
        // `http://alloy:4318/v1/traces/v1/metrics`, and metrics would be dropped
        // by the collector with nothing in MOA reporting a problem.
        for signal in ["/v1/traces", "/v1/metrics", "/v1/logs"] {
            let endpoint = format!("http://alloy:4318{signal}");
            let error = otlp_signal_endpoint(&endpoint, OtlpProtocol::Http, OtlpSignal::Metrics)
                .expect_err("a signal-specific endpoint must be refused");
            assert!(
                error.to_string().contains(signal),
                "the refusal must name the offending suffix `{signal}`, got: {error}"
            );
        }
    }
}

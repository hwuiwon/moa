//! Tracing and OpenTelemetry bootstrap helpers for MOA binaries.

use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::tonic_types::metadata::MetadataMap;
use opentelemetry_otlp::{
    Protocol, SpanExporter, WithExportConfig, WithHttpConfig, WithTonicConfig,
};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::runtime_metrics::init_metrics;
use moa_config::MetricsConfig;
use moa_config::MetricsExporter;
use moa_config::MoaConfig;
use moa_config::ObservabilityConfig;
use moa_config::OtlpProtocol;
use moa_core::{error::MoaError, error::Result};

/// Owns the configured OTLP providers for the process lifetime.
///
/// Both signals live here. A guard that owned only the tracer would flush spans
/// at shutdown and silently discard whatever the metric exporter had buffered,
/// which is exactly the window in which the most interesting metrics (the drain,
/// the backlog, the final counters) are produced.
#[derive(Debug, Default)]
pub struct TelemetryGuard {
    provider: Option<SdkTracerProvider>,
    meter_provider: Option<SdkMeterProvider>,
}

impl TelemetryGuard {
    /// Creates an empty telemetry guard when OTLP export is disabled.
    pub fn disabled() -> Self {
        Self {
            provider: None,
            meter_provider: None,
        }
    }

    /// Flushes and shuts both providers down, in the order that keeps data.
    ///
    /// Consuming, so a binary cannot call it and then keep exporting into a
    /// provider that has already been shut down. Metrics are flushed before
    /// traces because the metric exporter aggregates over an interval and holds
    /// the most unexported state; tracing is span-per-event and its batch
    /// exporter is closer to caught up at any instant.
    ///
    /// [`Drop`] remains a best-effort backstop for paths that exit without
    /// calling this, but it cannot report failures and runs at an unpredictable
    /// point, so it is not a substitute.
    pub fn shutdown(mut self) {
        if let Some(meter_provider) = self.meter_provider.take()
            && let Err(error) = meter_provider.shutdown()
        {
            tracing::warn!(%error, "OTLP meter provider shutdown failed; metrics may be lost");
        }
        if let Some(provider) = self.provider.take()
            && let Err(error) = provider.shutdown()
        {
            tracing::warn!(%error, "OTLP tracer provider shutdown failed; traces may be lost");
        }
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(meter_provider) = self.meter_provider.take() {
            let _ = meter_provider.shutdown();
        }
        if let Some(provider) = self.provider.take() {
            let _ = provider.shutdown();
        }
    }
}

/// API-controlled telemetry settings layered on top of config-driven observability.
#[derive(Debug, Clone, Default)]
pub struct TelemetryConfig {
    /// Emits structured JSON logs to stdout instead of the human-readable console formatter.
    pub json_stdout: bool,
}

/// Initializes tracing with optional OTLP export and returns a guard that owns active writers.
pub fn init_observability(
    config: &MoaConfig,
    telemetry: &TelemetryConfig,
) -> Result<TelemetryGuard> {
    // Install the W3C propagator regardless of whether OTLP export is enabled so
    // inbound `traceparent` extraction and outbound injection behave uniformly.
    crate::propagation::init_trace_propagation();

    let console_layer = if telemetry.json_stdout {
        Some(
            tracing_subscriber::fmt::layer()
                .json()
                .with_target(true)
                .with_current_span(true)
                .flatten_event(true)
                .with_filter(configured_env_filter())
                .boxed(),
        )
    } else {
        Some(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_filter(configured_env_filter())
                .boxed(),
        )
    };

    // The resource identity is built once and used for BOTH signals. Traces and
    // metrics that disagree about service.name, deployment.environment or
    // service.version cannot be correlated at all, and the disagreement is
    // invisible until someone tries to join them in a query.
    let resource = build_resource(&config.observability);
    let otlp_base = config.observability.otlp_endpoint.as_deref();
    let otlp_protocol = config.observability.otlp_protocol;

    if !config.observability.enabled {
        let _ = tracing_subscriber::registry()
            .with(console_layer)
            .try_init();
        // `observability.enabled` is the master switch for OTLP export, and it
        // governs BOTH signals. Installing an OTLP meter provider here would
        // have every default-config process pushing at a collector the operator
        // never said existed, while traces stayed correctly off - the same
        // one-signal-on, one-signal-off split this task exists to remove. The
        // scrape and disabled exporters are unaffected: neither needs a
        // collector.
        let metrics = match config.metrics.exporter {
            MetricsExporter::Otlp => {
                tracing::info!(
                    "runtime metrics are not exported: metrics.exporter is `otlp` but \
                     observability.enabled is false, so there is no configured collector"
                );
                &MetricsConfig {
                    exporter: MetricsExporter::Disabled,
                    prometheus_listen: None,
                }
            }
            _ => &config.metrics,
        };
        let meter_provider = init_metrics(metrics, otlp_base, otlp_protocol, resource)?;
        return Ok(TelemetryGuard {
            provider: None,
            meter_provider,
        });
    }

    let exporter = build_span_exporter(&config.observability)?;
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource.clone())
        .with_sampler(build_sampler(config.observability.sample_rate))
        .build();
    let tracer = provider.tracer(config.observability.service_name.clone());
    let otel_layer = tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_filter(LevelFilter::INFO);

    let _ = tracing_subscriber::registry()
        .with(console_layer)
        .with(otel_layer)
        .try_init();
    let meter_provider = init_metrics(&config.metrics, otlp_base, otlp_protocol, resource)?;

    Ok(TelemetryGuard {
        provider: Some(provider),
        meter_provider,
    })
}

fn configured_env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_env_filter_directive()))
}

fn default_env_filter_directive() -> &'static str {
    // async-openai logs deserialization failures for stream event types it
    // does not model yet. moa-providers already handles known-safe unknown
    // events defensively, so surfacing those SDK internals as process-level
    // errors creates false negatives in real API runs.
    "warn,async_openai::error=off"
}

fn build_span_exporter(config: &ObservabilityConfig) -> Result<SpanExporter> {
    match config.otlp_protocol {
        OtlpProtocol::Grpc => {
            let mut exporter = SpanExporter::builder().with_tonic();
            if let Some(endpoint) = config.otlp_endpoint.as_ref() {
                exporter = exporter.with_endpoint(moa_config::otlp_signal_endpoint(
                    endpoint,
                    OtlpProtocol::Grpc,
                    moa_config::OtlpSignal::Traces,
                )?);
            }
            if !config.otlp_headers.is_empty() {
                exporter = exporter.with_metadata(build_grpc_metadata(&config.otlp_headers)?);
            }
            exporter
                .build()
                .map_err(|error| MoaError::ProviderError(error.to_string()))
        }
        OtlpProtocol::Http => {
            let mut exporter = SpanExporter::builder()
                .with_http()
                .with_protocol(Protocol::HttpBinary);
            if let Some(endpoint) = config.otlp_endpoint.as_ref() {
                exporter = exporter.with_endpoint(moa_config::otlp_signal_endpoint(
                    endpoint,
                    OtlpProtocol::Http,
                    moa_config::OtlpSignal::Traces,
                )?);
            }
            if !config.otlp_headers.is_empty() {
                exporter = exporter.with_headers(config.otlp_headers.clone());
            }
            exporter
                .build()
                .map_err(|error| MoaError::ProviderError(error.to_string()))
        }
    }
}

fn build_resource(config: &ObservabilityConfig) -> Resource {
    let mut attributes = Vec::new();

    if let Some(environment) = &config.environment {
        attributes.push(KeyValue::new("deployment.environment", environment.clone()));
    }
    if let Some(release) = &config.release {
        attributes.push(KeyValue::new("service.version", release.clone()));
    }

    Resource::builder()
        .with_service_name(config.service_name.clone())
        .with_attributes(attributes)
        .build()
}

fn build_sampler(sample_rate: f64) -> Sampler {
    let normalized = if sample_rate.is_finite() {
        sample_rate.clamp(0.0, 1.0)
    } else {
        1.0
    };

    if normalized <= 0.0 {
        Sampler::AlwaysOff
    } else if normalized < 1.0 {
        Sampler::TraceIdRatioBased(normalized)
    } else {
        Sampler::AlwaysOn
    }
}

fn build_grpc_metadata(headers: &std::collections::HashMap<String, String>) -> Result<MetadataMap> {
    Ok(MetadataMap::from_headers(build_http_headers(headers)?))
}

fn build_http_headers(headers: &std::collections::HashMap<String, String>) -> Result<HeaderMap> {
    let mut header_map = HeaderMap::new();
    for (name, value) in headers {
        let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
            MoaError::ConfigError(format!("invalid OTLP header name `{name}`: {error}"))
        })?;
        let header_value = HeaderValue::from_str(value).map_err(|error| {
            MoaError::ConfigError(format!("invalid OTLP header value for `{name}`: {error}"))
        })?;
        header_map.insert(header_name, header_value);
    }
    Ok(header_map)
}

#[cfg(test)]
mod tests {
    use opentelemetry::{Key, Value};

    use super::*;

    #[test]
    fn resource_includes_environment_and_release() {
        let resource = build_resource(&ObservabilityConfig {
            service_name: "moa".to_string(),
            environment: Some("production".to_string()),
            release: Some("v1.2.3".to_string()),
            ..ObservabilityConfig::default()
        });

        assert_eq!(
            resource.get(&Key::new("service.name")),
            Some(Value::from("moa"))
        );
        assert_eq!(
            resource.get(&Key::new("deployment.environment")),
            Some(Value::from("production"))
        );
        assert_eq!(
            resource.get(&Key::new("service.version")),
            Some(Value::from("v1.2.3"))
        );
    }

    #[test]
    fn init_observability_disabled_exports_neither_signal() {
        // Pins: `observability.enabled = false` turns OFF both signals. The
        // metrics exporter now defaults to `otlp`, so without this the default
        // configuration would push runtime metrics at a collector nobody
        // configured while traces stayed off - one signal on and one off, which
        // is the exact split this task removes.
        let config = MoaConfig::default();
        assert_eq!(
            config.metrics.exporter,
            moa_config::MetricsExporter::Otlp,
            "precondition: the default exporter must be the push one, or this test \
             proves nothing about the interaction"
        );

        let guard = init_observability(&config, &TelemetryConfig::default())
            .expect("disabled observability should initialize");

        assert!(
            guard.provider.is_none(),
            "no tracer provider when OTLP is off"
        );
        assert!(
            guard.meter_provider.is_none(),
            "no meter provider either: OTLP export is disabled, so there is no collector"
        );
    }

    #[tokio::test]
    async fn the_guard_owns_the_meter_provider_whenever_otlp_metrics_are_selected() {
        // Pins: the guard actually HOLDS the meter provider. A shutdown that
        // flushes traces and not metrics is invisible in any assertion about the
        // shutdown call itself - the call succeeds either way - so what has to be
        // pinned is ownership. Without the provider here there is nothing to
        // flush, and every metric buffered in the final export interval (the
        // drain, the backlog, the closing counters) is lost silently.
        // Async because the default transport is gRPC, and building a tonic
        // exporter needs a live reactor. That is production's situation too; a
        // sync test here would only prove the HTTP arm.
        let mut config = MoaConfig::default();
        config.observability.enabled = true;
        config.metrics.exporter = moa_config::MetricsExporter::Otlp;

        let guard = init_observability(&config, &TelemetryConfig::default())
            .expect("otlp metrics should initialize");

        assert!(
            guard.meter_provider.is_some(),
            "the telemetry guard must own the meter provider so shutdown can flush it; \
             exporter was {:?}",
            config.metrics.exporter
        );
    }

    #[tokio::test]
    async fn the_guard_owns_no_meter_provider_when_metrics_are_disabled() {
        // Negative control for the test above: without it, a guard that stored a
        // provider unconditionally would satisfy the assertion while exporting
        // nothing, and the pin would prove only that a field can be non-None.
        let mut config = MoaConfig::default();
        config.observability.enabled = true;
        config.metrics.exporter = moa_config::MetricsExporter::Disabled;

        let guard = init_observability(&config, &TelemetryConfig::default())
            .expect("disabled metrics should initialize");

        assert!(
            guard.meter_provider.is_none(),
            "a disabled exporter must own no provider to flush"
        );
    }

    #[test]
    fn grpc_metadata_uses_header_values() {
        let metadata = build_grpc_metadata(&std::collections::HashMap::from([
            (
                "authorization".to_string(),
                "Basic cGstbGYteHh4eHg6c2stbGYteHh4eHg=".to_string(),
            ),
            ("x-moa-tenant".to_string(), "tenant-a".to_string()),
        ]))
        .expect("metadata should build");

        assert_eq!(
            metadata
                .get("authorization")
                .expect("authorization header present")
                .to_str()
                .expect("authorization header is valid ASCII"),
            "Basic cGstbGYteHh4eHg6c2stbGYteHh4eHg="
        );
        assert_eq!(
            metadata
                .get("x-moa-tenant")
                .expect("tenant header present")
                .to_str()
                .expect("tenant header is valid ASCII"),
            "tenant-a"
        );
    }
}

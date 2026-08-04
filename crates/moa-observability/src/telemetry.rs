//! Tracing and OpenTelemetry bootstrap helpers for MOA binaries.

use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::tonic_types::metadata::MetadataMap;
use opentelemetry_otlp::{
    LogExporter, Protocol, SpanExporter, WithExportConfig, WithHttpConfig, WithTonicConfig,
};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
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
/// All three OTLP signal providers live here. A guard that owned only the tracer
/// would flush spans at shutdown and silently discard buffered metrics and logs,
/// which is exactly the window in which the most interesting drain and failure
/// telemetry is produced.
#[derive(Debug, Default)]
pub struct TelemetryGuard {
    provider: Option<SdkTracerProvider>,
    meter_provider: Option<SdkMeterProvider>,
    logger_provider: Option<SdkLoggerProvider>,
}

impl TelemetryGuard {
    /// Creates an empty telemetry guard when OTLP export is disabled.
    pub fn disabled() -> Self {
        Self {
            provider: None,
            meter_provider: None,
            logger_provider: None,
        }
    }

    /// Flushes and shuts all providers down, in the order that keeps data.
    ///
    /// Consuming, so a binary cannot call it and then keep exporting into a
    /// provider that has already been shut down. Metrics are flushed before
    /// traces because the metric exporter aggregates over an interval and holds
    /// the most unexported state. Logs shut down last so warnings from metric or
    /// trace shutdown still have an active structured-log path.
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
        if let Some(logger_provider) = self.logger_provider.take()
            && let Err(error) = logger_provider.shutdown()
        {
            tracing::warn!(%error, "OTLP logger provider shutdown failed; logs may be lost");
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
        if let Some(logger_provider) = self.logger_provider.take() {
            let _ = logger_provider.shutdown();
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

    // The resource identity is built once and used for all three signals.
    // Traces, metrics, and logs that disagree about service.name,
    // deployment.environment, or service.version cannot be correlated at all,
    // and the disagreement is invisible until someone tries to join them.
    let resource = build_resource(&config.observability);
    let otlp_base = config.observability.otlp_endpoint.as_deref();
    let otlp_protocol = config.observability.otlp_protocol;

    if otlp_base.is_none() {
        let _ = tracing_subscriber::registry()
            .with(console_layer)
            .try_init();
        // Collector endpoint presence is the sole OTLP switch and governs all
        // signals. The scrape and disabled metric exporters are unaffected:
        // neither needs a collector.
        let metrics = match config.metrics.exporter {
            MetricsExporter::Otlp => {
                tracing::info!(
                    "runtime metrics are not exported: metrics.exporter is `otlp` but \
                     observability.otlp_endpoint is absent, so there is no configured collector"
                );
                &MetricsConfig {
                    exporter: MetricsExporter::Disabled,
                    prometheus_listen: None,
                }
            }
            _ => &config.metrics,
        };
        let meter_provider = init_metrics(
            metrics,
            otlp_base,
            otlp_protocol,
            &config.observability.otlp_headers,
            resource,
        )?;
        return Ok(TelemetryGuard {
            provider: None,
            meter_provider,
            logger_provider: None,
        });
    }

    let span_exporter = build_span_exporter(&config.observability)?;
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(span_exporter)
        .with_resource(resource.clone())
        .with_sampler(build_sampler(config.observability.sample_rate))
        .build();
    let tracer = provider.tracer(config.observability.service_name.clone());
    let span_layer = tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_filter(LevelFilter::INFO);
    let log_exporter = build_log_exporter(&config.observability)?;
    let logger_provider = SdkLoggerProvider::builder()
        .with_batch_exporter(log_exporter)
        .with_resource(resource.clone())
        .build();
    let log_layer =
        OpenTelemetryTracingBridge::new(&logger_provider).with_filter(configured_otlp_log_filter());

    let _ = tracing_subscriber::registry()
        .with(console_layer)
        .with(span_layer)
        .with(log_layer)
        .try_init();
    let meter_provider = init_metrics(
        &config.metrics,
        otlp_base,
        otlp_protocol,
        &config.observability.otlp_headers,
        resource,
    )?;

    Ok(TelemetryGuard {
        provider: Some(provider),
        meter_provider,
        logger_provider: Some(logger_provider),
    })
}

fn configured_env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_env_filter_directive()))
}

fn configured_otlp_log_filter() -> EnvFilter {
    let mut filter = configured_env_filter();
    // Exporter dependencies can log failures through `tracing`. Sending those
    // records back through the same failing exporter creates a telemetry loop.
    // Static directives cannot fail in practice; retaining the prior filter on
    // a parse failure is safer than making logging initialization fallible.
    for directive in [
        "opentelemetry=off",
        "opentelemetry_otlp=off",
        "hyper=off",
        "h2=off",
        "tonic=off",
        "reqwest=off",
    ] {
        if let Ok(directive) = directive.parse() {
            filter = filter.add_directive(directive);
        }
    }
    filter
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

fn build_log_exporter(config: &ObservabilityConfig) -> Result<LogExporter> {
    match config.otlp_protocol {
        OtlpProtocol::Grpc => {
            let mut exporter = LogExporter::builder().with_tonic();
            if let Some(endpoint) = config.otlp_endpoint.as_ref() {
                exporter = exporter.with_endpoint(moa_config::otlp_signal_endpoint(
                    endpoint,
                    OtlpProtocol::Grpc,
                    moa_config::OtlpSignal::Logs,
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
            let mut exporter = LogExporter::builder()
                .with_http()
                .with_protocol(Protocol::HttpBinary);
            if let Some(endpoint) = config.otlp_endpoint.as_ref() {
                exporter = exporter.with_endpoint(moa_config::otlp_signal_endpoint(
                    endpoint,
                    OtlpProtocol::Http,
                    moa_config::OtlpSignal::Logs,
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
    build_resource_with_instance(
        config,
        std::env::var("MOA_SERVICE_INSTANCE_ID")
            .ok()
            .filter(|value| !value.trim().is_empty()),
    )
}

fn build_resource_with_instance(
    config: &ObservabilityConfig,
    service_instance_id: Option<String>,
) -> Resource {
    let mut attributes = Vec::new();

    if let Some(environment) = &config.environment {
        attributes.push(KeyValue::new("deployment.environment", environment.clone()));
    }
    if let Some(release) = &config.release {
        attributes.push(KeyValue::new("service.version", release.clone()));
    }
    if let Some(service_instance_id) = service_instance_id {
        attributes.push(KeyValue::new("service.instance.id", service_instance_id));
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

    let root_sampler = if normalized <= 0.0 {
        Sampler::AlwaysOff
    } else if normalized < 1.0 {
        Sampler::TraceIdRatioBased(normalized)
    } else {
        Sampler::AlwaysOn
    };

    Sampler::ParentBased(Box::new(root_sampler))
}

/// Builds validated gRPC metadata shared by trace, metric, and log OTLP exporters.
pub(crate) fn build_grpc_metadata(
    headers: &std::collections::HashMap<String, String>,
) -> Result<MetadataMap> {
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
    use opentelemetry::trace::{
        SamplingDecision, SpanContext, SpanId, SpanKind, TraceContextExt, TraceFlags, TraceId,
        TraceState,
    };
    use opentelemetry::{Context, Key, Value};
    use opentelemetry_sdk::trace::ShouldSample;

    use super::*;

    fn sampling_decision(
        sampler: &Sampler,
        parent_context: Option<&Context>,
        trace_id: TraceId,
    ) -> SamplingDecision {
        sampler
            .should_sample(
                parent_context,
                trace_id,
                "sampler-test",
                &SpanKind::Internal,
                &[],
                &[],
            )
            .decision
    }

    fn remote_parent_context(sampled: bool) -> Context {
        let trace_flags = if sampled {
            TraceFlags::SAMPLED
        } else {
            TraceFlags::default()
        };
        Context::new().with_remote_span_context(SpanContext::new(
            TraceId::from(1),
            SpanId::from(1),
            trace_flags,
            true,
            TraceState::default(),
        ))
    }

    #[test]
    fn sampler_honors_parent_decisions_for_every_root_policy() {
        // Pins: an upstream sampled or unsampled decision wins even when this
        // service's normalized root policy would make the opposite decision.
        let sampled_parent = remote_parent_context(true);
        let unsampled_parent = remote_parent_context(false);

        for sample_rate in [0.0, 0.5, 1.0] {
            let sampler = build_sampler(sample_rate);
            assert_eq!(
                sampling_decision(&sampler, Some(&sampled_parent), TraceId::from(u128::MAX)),
                SamplingDecision::RecordAndSample,
                "sampled parent must override the root policy at sample_rate={sample_rate}"
            );
            assert_eq!(
                sampling_decision(&sampler, Some(&unsampled_parent), TraceId::from(1)),
                SamplingDecision::Drop,
                "unsampled parent must override the root policy at sample_rate={sample_rate}"
            );
        }
    }

    #[test]
    fn sampler_preserves_normalized_policy_for_root_spans() {
        // Pins: wrapping the sampler to honor parents does not change the
        // AlwaysOff, ratio-based, or AlwaysOn decisions for root spans.
        assert_eq!(
            sampling_decision(&build_sampler(0.0), None, TraceId::from(1)),
            SamplingDecision::Drop
        );
        assert_eq!(
            sampling_decision(&build_sampler(0.5), None, TraceId::from(1)),
            SamplingDecision::RecordAndSample
        );
        assert_eq!(
            sampling_decision(&build_sampler(0.5), None, TraceId::from(u128::MAX)),
            SamplingDecision::Drop
        );
        assert_eq!(
            sampling_decision(&build_sampler(1.0), None, TraceId::from(u128::MAX)),
            SamplingDecision::RecordAndSample
        );
    }

    #[test]
    fn resource_shares_environment_release_and_instance_identity() {
        // Pins: traces, metrics, and logs clone one resource whose instance
        // identity separates otherwise identical replicas in the backend.
        let resource = build_resource_with_instance(
            &ObservabilityConfig {
                service_name: "moa".to_string(),
                environment: Some("production".to_string()),
                release: Some("v1.2.3".to_string()),
                ..ObservabilityConfig::default()
            },
            Some("pod-uid-a".to_string()),
        );

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
        assert_eq!(
            resource.get(&Key::new("service.instance.id")),
            Some(Value::from("pod-uid-a"))
        );
    }

    #[test]
    fn missing_endpoint_exports_no_otlp_signals() {
        // Pins: endpoint absence turns off all OTLP signals. The metrics exporter
        // defaults to `otlp`, so it must not push at the SDK's localhost default
        // while traces and logs remain off.
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
        assert!(
            guard.logger_provider.is_none(),
            "no logger provider when there is no collector endpoint"
        );
    }

    #[tokio::test]
    async fn endpoint_installs_and_guard_owns_all_otlp_signal_providers() {
        // Pins: one endpoint installs trace, metric, and log providers and the
        // guard holds all three so process shutdown can flush every signal.
        // Async because the default transport is gRPC, and building a tonic
        // exporter needs a live reactor. That is production's situation too; a
        // sync test here would only prove the HTTP arm.
        let mut config = MoaConfig::default();
        config.observability.otlp_endpoint = Some("http://127.0.0.1:4317".to_string());
        config.metrics.exporter = moa_config::MetricsExporter::Otlp;

        let guard = init_observability(&config, &TelemetryConfig::default())
            .expect("endpoint-driven OTLP should initialize");

        assert!(
            guard.provider.is_some(),
            "the guard must own the tracer provider"
        );
        assert!(
            guard.meter_provider.is_some(),
            "the telemetry guard must own the meter provider so shutdown can flush it; \
             exporter was {:?}",
            config.metrics.exporter
        );
        assert!(
            guard.logger_provider.is_some(),
            "the guard must own the logger provider so shutdown can flush logs"
        );
    }

    #[tokio::test]
    async fn the_guard_owns_no_meter_provider_when_metrics_are_disabled() {
        // Negative control for the test above: without it, a guard that stored a
        // provider unconditionally would satisfy the assertion while exporting
        // nothing, and the pin would prove only that a field can be non-None.
        let mut config = MoaConfig::default();
        config.observability.otlp_endpoint = Some("http://127.0.0.1:4317".to_string());
        config.metrics.exporter = moa_config::MetricsExporter::Disabled;

        let guard = init_observability(&config, &TelemetryConfig::default())
            .expect("disabled metrics should initialize");

        assert!(
            guard.meter_provider.is_none(),
            "a disabled exporter must own no provider to flush"
        );
        assert!(guard.provider.is_some(), "traces still use the endpoint");
        assert!(
            guard.logger_provider.is_some(),
            "logs still use the endpoint"
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

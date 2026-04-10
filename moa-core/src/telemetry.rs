//! Tracing and OpenTelemetry bootstrap helpers for MOA binaries.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::MoaConfig;
use crate::error::{MoaError, Result};

/// Keeps the configured OTLP tracer provider alive for the process lifetime.
#[derive(Debug, Default)]
pub struct TelemetryGuard {
    provider: Option<SdkTracerProvider>,
}

impl TelemetryGuard {
    /// Creates an empty telemetry guard when OTLP export is disabled.
    pub fn disabled() -> Self {
        Self { provider: None }
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take() {
            let _ = provider.shutdown();
        }
    }
}

/// Initializes tracing with optional OTLP export and returns a guard that owns the tracer provider.
pub fn init_observability(config: &MoaConfig) -> Result<TelemetryGuard> {
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_filter(LevelFilter::WARN);

    if !config.observability.enabled {
        let _ = tracing_subscriber::registry().with(fmt_layer).try_init();
        return Ok(TelemetryGuard::disabled());
    }

    let mut exporter = opentelemetry_otlp::SpanExporter::builder().with_tonic();
    if let Some(endpoint) = config.observability.otlp_endpoint.as_ref() {
        exporter = exporter.with_endpoint(endpoint);
    }
    let exporter = exporter
        .build()
        .map_err(|error| MoaError::ProviderError(error.to_string()))?;

    let resource = Resource::builder()
        .with_service_name(config.observability.service_name.clone())
        .build();
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();
    let tracer = provider.tracer(config.observability.service_name.clone());
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

    let _ = tracing_subscriber::registry()
        .with(fmt_layer)
        .with(otel_layer)
        .try_init();

    Ok(TelemetryGuard {
        provider: Some(provider),
    })
}

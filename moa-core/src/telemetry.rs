//! Tracing and OpenTelemetry bootstrap helpers for MOA binaries.

use std::fs::OpenOptions;
use std::path::PathBuf;

use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::tonic_types::metadata::MetadataMap;
use opentelemetry_otlp::{
    Protocol, SpanExporter, WithExportConfig, WithHttpConfig, WithTonicConfig,
};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::{MoaConfig, ObservabilityConfig, OtlpProtocol};
use crate::error::{MoaError, Result};

/// Keeps the configured OTLP tracer provider alive for the process lifetime.
#[derive(Debug, Default)]
pub struct TelemetryGuard {
    provider: Option<SdkTracerProvider>,
    _log_writer_guard: Option<tracing_appender::non_blocking::WorkerGuard>,
}

impl TelemetryGuard {
    /// Creates an empty telemetry guard when OTLP export is disabled.
    pub fn disabled() -> Self {
        Self {
            provider: None,
            _log_writer_guard: None,
        }
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take() {
            let _ = provider.shutdown();
        }
    }
}

/// CLI-controlled telemetry settings layered on top of config-driven observability.
#[derive(Debug, Clone, Default)]
pub struct TelemetryConfig {
    /// Enables debug-level file logging.
    pub debug: bool,
    /// Optional explicit log file path.
    pub log_file: Option<PathBuf>,
}

/// Returns the default MOA operator log file path.
pub fn default_log_path() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".moa").join("moa.log"),
        None => PathBuf::from(".moa").join("moa.log"),
    }
}

/// Initializes tracing with optional OTLP export and returns a guard that owns active writers.
pub fn init_observability(
    config: &MoaConfig,
    telemetry: &TelemetryConfig,
) -> Result<TelemetryGuard> {
    let console_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_filter(LevelFilter::WARN);
    let env_filter = Some(
        EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(default_env_filter_directive())),
    );

    let (file_layer, log_writer_guard) = if telemetry.debug || telemetry.log_file.is_some() {
        let path = telemetry.log_file.clone().unwrap_or_else(default_log_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| MoaError::ConfigError(error.to_string()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| MoaError::ConfigError(error.to_string()))?;
        let (writer, guard) = tracing_appender::non_blocking(file);
        let file_filter = if telemetry.debug {
            LevelFilter::DEBUG
        } else {
            LevelFilter::INFO
        };
        (
            Some(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_target(true)
                    .with_writer(writer)
                    .with_filter(file_filter),
            ),
            Some(guard),
        )
    } else {
        (None, None)
    };

    if !config.observability.enabled {
        let _ = tracing_subscriber::registry()
            .with(env_filter)
            .with(console_layer)
            .with(file_layer)
            .try_init();
        return Ok(TelemetryGuard {
            provider: None,
            _log_writer_guard: log_writer_guard,
        });
    }

    let exporter = build_span_exporter(&config.observability)?;
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(build_resource(&config.observability))
        .with_sampler(build_sampler(config.observability.sample_rate))
        .build();
    let tracer = provider.tracer(config.observability.service_name.clone());
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

    let _ = tracing_subscriber::registry()
        .with(env_filter)
        .with(console_layer)
        .with(file_layer)
        .with(otel_layer)
        .try_init();

    Ok(TelemetryGuard {
        provider: Some(provider),
        _log_writer_guard: log_writer_guard,
    })
}

fn default_env_filter_directive() -> &'static str {
    // async-openai logs deserialization failures for stream event types it
    // does not model yet. moa-providers already handles known-safe unknown
    // events defensively, so surfacing those SDK internals as process-level
    // errors creates false negatives in real CLI runs.
    "warn,async_openai::error=off"
}

fn build_span_exporter(config: &ObservabilityConfig) -> Result<SpanExporter> {
    match config.otlp_protocol {
        OtlpProtocol::Grpc => {
            let mut exporter = SpanExporter::builder().with_tonic();
            if let Some(endpoint) = config.otlp_endpoint.as_ref() {
                exporter = exporter.with_endpoint(endpoint);
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
                exporter = exporter.with_endpoint(endpoint);
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

    if let Some(environment) = config.environment.as_ref() {
        attributes.push(KeyValue::new("deployment.environment", environment.clone()));
        attributes.push(KeyValue::new("langfuse.environment", environment.clone()));
    }
    if let Some(release) = config.release.as_ref() {
        attributes.push(KeyValue::new("service.version", release.clone()));
        attributes.push(KeyValue::new("langfuse.release", release.clone()));
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
        assert_eq!(
            resource.get(&Key::new("langfuse.environment")),
            Some(Value::from("production"))
        );
        assert_eq!(
            resource.get(&Key::new("langfuse.release")),
            Some(Value::from("v1.2.3"))
        );
    }

    #[test]
    fn init_observability_disabled_returns_guard() {
        let config = MoaConfig::default();
        let guard = init_observability(&config, &TelemetryConfig::default())
            .expect("disabled observability should initialize");
        assert!(guard.provider.is_none());
    }

    #[test]
    fn grpc_metadata_uses_header_values() {
        let metadata = build_grpc_metadata(&std::collections::HashMap::from([
            (
                "authorization".to_string(),
                "Basic cGstbGYteHh4eHg6c2stbGYteHh4eHg=".to_string(),
            ),
            ("x-langfuse-ingestion-version".to_string(), "4".to_string()),
        ]))
        .expect("metadata should build");

        assert!(metadata.get("authorization").is_some());
        assert!(metadata.get("x-langfuse-ingestion-version").is_some());
    }
}

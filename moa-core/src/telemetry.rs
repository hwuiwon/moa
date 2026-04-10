//! Tracing and OpenTelemetry bootstrap helpers for MOA binaries.

use std::fs::OpenOptions;
use std::path::PathBuf;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::EnvFilter;
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
    let env_filter = EnvFilter::try_from_default_env().ok();

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

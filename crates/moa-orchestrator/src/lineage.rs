//! Lineage sink selection for the Restate-backed orchestrator.

use std::sync::Arc;

use moa_config::MoaConfig;
use moa_core::{error::MoaError, error::Result, traits::LineageHandle, traits::NullLineageHandle};

/// Runtime lineage sink and optional background writer handle.
pub struct LineageSinkRuntime {
    /// Hot-path lineage handle passed into the context pipeline.
    pub handle: Arc<dyn LineageHandle>,
    /// Durable writer handle when the selected sink owns a background writer.
    pub writer: Option<Arc<moa_lineage_sink::WriterHandle>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineageSinkMode {
    Null,
    Otel,
    /// Durable DB sink; the row backend follows `[clickhouse]` config presence.
    Postgres,
    /// Durable DB sink that requires `[clickhouse]` to be configured.
    ClickHouse,
}

impl LineageSinkMode {
    fn from_env_value(value: Option<&str>) -> Result<Self> {
        let normalized = value.map(str::trim).filter(|value| !value.is_empty());
        match normalized.map(str::to_ascii_lowercase).as_deref() {
            Some("postgres") => Ok(Self::Postgres),
            Some("clickhouse") => Ok(Self::ClickHouse),
            Some("otel") => Ok(Self::Otel),
            Some("null") | None => Ok(Self::Null),
            Some(other) => Err(MoaError::ConfigError(format!(
                "unknown MOA_LINEAGE_SINK value: {other}; expected postgres|clickhouse|null|otel"
            ))),
        }
    }
}

/// Builds the lineage sink selected by `MOA_LINEAGE_SINK`.
pub async fn build_lineage_sink(
    config: &MoaConfig,
    pool: sqlx::PgPool,
) -> Result<LineageSinkRuntime> {
    let env_value = std::env::var("MOA_LINEAGE_SINK").ok();
    build_lineage_sink_from_env_value(config, pool, env_value.as_deref()).await
}

/// Builds the lineage sink selected by a caller-provided env var value.
///
/// This is the same selector as [`build_lineage_sink`], with the env read made
/// explicit so tests do not mutate process-global environment.
pub async fn build_lineage_sink_from_env_value(
    config: &MoaConfig,
    pool: sqlx::PgPool,
    env_value: Option<&str>,
) -> Result<LineageSinkRuntime> {
    let mode = LineageSinkMode::from_env_value(env_value)?;
    match mode {
        LineageSinkMode::Postgres | LineageSinkMode::ClickHouse => {
            if mode == LineageSinkMode::ClickHouse && config.clickhouse.is_none() {
                return Err(MoaError::ConfigError(
                    "MOA_LINEAGE_SINK=clickhouse requires the [clickhouse] config section \
                     (or MOA_CLICKHOUSE_URL)"
                        .to_string(),
                ));
            }
            let store =
                moa_lineage_sink::LineageStore::from_config(config.clickhouse.as_ref(), pool);
            let backend = store.backend_name();
            store.ensure_schema().await.map_err(|error| {
                MoaError::StorageError(format!("lineage schema setup failed: {error}"))
            })?;
            // Refuse to start on the ClickHouse backend when any compliance tenant is
            // enabled: that backend cannot hash-chain compliance rows, and a silent
            // downgrade of `moa lineage verify` is worse than a loud startup failure.
            store.guard_compliance_backend().await.map_err(|error| {
                MoaError::ConfigError(format!("lineage backend refused to start: {error}"))
            })?;
            let sink_config = moa_lineage_sink::MpscSinkConfig::from(&config.observability.lineage);
            let (sink, writer) = moa_lineage_sink::MpscSink::spawn(sink_config, store)
                .await
                .map_err(|error| {
                    MoaError::StorageError(format!("lineage writer startup failed: {error}"))
                })?;
            tracing::info!(backend, "lineage sink: durable (MpscSink)");
            Ok(LineageSinkRuntime {
                handle: Arc::new(sink),
                writer: Some(Arc::new(writer)),
            })
        }
        LineageSinkMode::Null => {
            tracing::info!("lineage sink: null");
            Ok(LineageSinkRuntime {
                handle: Arc::new(NullLineageHandle),
                writer: None,
            })
        }
        LineageSinkMode::Otel => {
            tracing::info!("lineage sink: otel span attributes only");
            Ok(LineageSinkRuntime {
                handle: Arc::new(moa_lineage_sink::OtelSink::new()),
                writer: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LineageSinkMode;

    #[test]
    fn lineage_sink_mode_defaults_unset_to_null_and_accepts_otel() {
        assert_eq!(
            LineageSinkMode::from_env_value(None).expect("unset should default to null"),
            LineageSinkMode::Null
        );
        assert_eq!(
            LineageSinkMode::from_env_value(Some("null")).expect("null should parse"),
            LineageSinkMode::Null
        );
        assert_eq!(
            LineageSinkMode::from_env_value(Some("otel")).expect("otel should parse"),
            LineageSinkMode::Otel
        );
    }

    #[test]
    fn lineage_sink_mode_accepts_postgres() {
        assert_eq!(
            LineageSinkMode::from_env_value(Some("postgres")).expect("postgres should parse"),
            LineageSinkMode::Postgres
        );
    }

    #[test]
    fn lineage_sink_mode_rejects_unknown_values() {
        let error = LineageSinkMode::from_env_value(Some("garbage"))
            .expect_err("garbage should be rejected");

        assert_eq!(
            error.to_string(),
            "configuration error: unknown MOA_LINEAGE_SINK value: garbage; expected postgres|clickhouse|null|otel"
        );
    }

    #[test]
    fn lineage_sink_mode_accepts_clickhouse() {
        assert_eq!(
            LineageSinkMode::from_env_value(Some("clickhouse")).expect("clickhouse should parse"),
            LineageSinkMode::ClickHouse
        );
    }
}

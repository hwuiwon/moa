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

impl LineageSinkRuntime {
    /// Returns a score-capable handle, or `None` when this sink cannot store scores.
    ///
    /// Only a sink that owns a durable background writer can claim a Behavior Lab
    /// score reached storage. The null sink drops events and the OTLP sink turns
    /// them into span attributes; both would let a trial report complete evidence
    /// with nothing in `analytics.scores` to read back. Capability is derived from
    /// the writer's presence rather than declared by the handle, so a sink cannot
    /// advertise durability it does not have.
    #[must_use]
    pub fn score_handle(&self) -> Option<ScoreLineageHandle> {
        self.writer
            .is_some()
            .then(|| ScoreLineageHandle(self.handle.clone()))
    }
}

/// A lineage handle proven to write scores into durable storage.
///
/// This type exists only so the trial finalizer cannot be constructed against a
/// telemetry-only sink. It is deliberately not constructible from a bare
/// [`LineageHandle`]: the only way to obtain one is
/// [`LineageSinkRuntime::score_handle`], which checks the durable writer.
#[derive(Clone)]
pub struct ScoreLineageHandle(Arc<dyn LineageHandle>);

impl ScoreLineageHandle {
    /// Returns the underlying durable lineage handle.
    #[must_use]
    pub fn handle(&self) -> &Arc<dyn LineageHandle> {
        &self.0
    }
}

impl std::fmt::Debug for ScoreLineageHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ScoreLineageHandle(durable)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineageSinkMode {
    Null,
    Otel,
    /// Durable DB sink writing `turn_lineage` to Postgres, whatever
    /// `[clickhouse]` says.
    Postgres,
    /// Durable DB sink writing `turn_lineage` to ClickHouse; requires
    /// `[clickhouse]` to be configured.
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
            // `postgres` means Postgres. Before this, both modes shared
            // `from_config`, which selects ClickHouse whenever `[clickhouse]` is
            // present - so an operator who explicitly named Postgres silently got
            // ClickHouse, with the startup log reporting the outcome correctly
            // and never flagging the contradiction. Worse, the compliance guard
            // below makes that override fail-closed only once a tenant enables
            // compliance, so the contradiction surfaced as a startup failure on a
            // config nobody had changed, detached in time from the decision that
            // caused it.
            let clickhouse = match mode {
                LineageSinkMode::ClickHouse => config.clickhouse.as_ref(),
                _ => None,
            };
            let store = moa_lineage_sink::LineageStore::from_config(clickhouse, pool);
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
    use super::{LineageSinkMode, LineageSinkRuntime};
    use moa_core::traits::NullLineageHandle;
    use std::sync::Arc;

    #[test]
    fn only_a_sink_with_a_durable_writer_yields_a_score_handle() {
        // Pins: null and OTLP-only lineage cannot masquerade as the durable product
        // score store. Without a background writer there is no score handle, so the
        // trial finalizer cannot be built and cannot claim score completion.
        let telemetry_only = LineageSinkRuntime {
            handle: Arc::new(NullLineageHandle),
            writer: None,
        };

        assert!(
            telemetry_only.score_handle().is_none(),
            "a writer-less sink must not be able to store product scores"
        );
    }

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

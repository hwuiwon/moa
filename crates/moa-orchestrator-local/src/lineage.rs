//! Lineage sink setup for the local orchestrator.

use crate::*;

pub(super) async fn build_lineage_sink(
    config: &MoaConfig,
    graph_pool: sqlx::PgPool,
) -> Result<(
    Arc<dyn moa_core::LineageHandle>,
    Option<Arc<moa_lineage_sink::WriterHandle>>,
)> {
    if !config.observability.lineage.enabled {
        return Ok((Arc::new(moa_core::NullLineageHandle), None));
    }

    ensure_schema(&graph_pool)
        .await
        .map_err(|error| MoaError::StorageError(format!("lineage schema setup failed: {error}")))?;
    let sink_config = MpscSinkConfig::from(&config.observability.lineage);
    let (sink, writer) = MpscSink::spawn(sink_config, graph_pool)
        .await
        .map_err(|error| {
            MoaError::StorageError(format!("lineage writer startup failed: {error}"))
        })?;
    Ok((Arc::new(sink), Some(Arc::new(writer))))
}

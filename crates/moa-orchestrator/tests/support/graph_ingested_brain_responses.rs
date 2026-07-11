//! Graph-ingestion assertion for visible brain responses.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use moa_core::{
    events::Event, types::events_stream::EventRecord, types::identifiers::SessionId,
    types::identifiers::StoragePartitionId,
};
use sqlx::PgPool;
use tokio::time::sleep;

/// Waits until graph ingestion has written nodes for every visible brain response.
pub async fn wait_for_ingested_brain_responses(
    pool: &PgPool,
    storage_partition_id: &StoragePartitionId,
    session_id: SessionId,
    events: &[EventRecord],
) -> Result<i64> {
    let turn_sequences = events
        .iter()
        .filter_map(|record| match record.event {
            Event::BrainResponse { .. } => Some(record.sequence_num.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if turn_sequences.is_empty() {
        bail!("no BrainResponse events found for session {session_id}")
    }

    for _attempt in 0..60 {
        let count = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT count(DISTINCT properties_summary->>'source_turn_seq')::bigint
            FROM moa.node_index
            WHERE storage_partition_id = $1
              AND valid_to IS NULL
              AND properties_summary->>'source_session_id' = $2
              AND properties_summary->>'source_turn_seq' = ANY($3::text[])
            "#,
        )
        .bind(storage_partition_id.to_string())
        .bind(session_id.to_string())
        .bind(&turn_sequences)
        .fetch_one(pool)
        .await
        .context("count graph-ingested brain responses")?;

        if count >= turn_sequences.len() as i64 {
            return Ok(count);
        }

        sleep(Duration::from_secs(1)).await;
    }

    bail!(
        "timed out waiting for graph ingestion for all BrainResponse events in session {session_id}"
    )
}

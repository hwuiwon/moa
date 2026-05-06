//! Shared graph-ingestion assertions for Restate e2e tests.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use moa_core::{Event, EventRecord, SessionId, WorkspaceId};
use sqlx::PgPool;
use tokio::time::sleep;

const DEFAULT_TEST_DATABASE_URL: &str = "postgres://moa_owner:dev@127.0.0.1:25432/moa";

/// Returns the Postgres URL used by local Restate e2e tests.
pub fn test_database_url() -> String {
    std::env::var("TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .unwrap_or_else(|_| DEFAULT_TEST_DATABASE_URL.to_string())
}

/// Waits until the asynchronous turn-ingestion invocation has written graph nodes.
pub async fn wait_for_ingested_turn(
    pool: &PgPool,
    workspace_id: &WorkspaceId,
    session_id: SessionId,
) -> Result<i64> {
    for _attempt in 0..60 {
        let count = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT count(*)::bigint
            FROM moa.node_index
            WHERE workspace_id = $1
              AND valid_to IS NULL
              AND properties_summary->>'source_session_id' = $2
            "#,
        )
        .bind(workspace_id.to_string())
        .bind(session_id.to_string())
        .fetch_one(pool)
        .await
        .context("count graph nodes for ingested turn")?;

        if count > 0 {
            return Ok(count);
        }

        sleep(Duration::from_secs(1)).await;
    }

    bail!("timed out waiting for graph ingestion for session {session_id}")
}

/// Waits until graph ingestion has written nodes for every visible brain response.
pub async fn wait_for_ingested_brain_responses(
    pool: &PgPool,
    workspace_id: &WorkspaceId,
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
            WHERE workspace_id = $1
              AND valid_to IS NULL
              AND properties_summary->>'source_session_id' = $2
              AND properties_summary->>'source_turn_seq' = ANY($3::text[])
            "#,
        )
        .bind(workspace_id.to_string())
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

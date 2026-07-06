//! Durable vector-backend sync queue for external vector projections.

use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::{Error, Result, VectorItem, embedding_row::EmbeddingRow};

/// Maximum outbox rows drained after a graph write commits.
pub const VECTOR_SYNC_POST_COMMIT_LIMIT: i64 = 64;

/// Operation persisted in `moa.vector_sync_outbox`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorSyncOperation {
    /// Re-read the current pgvector row and upsert it into the external backend.
    Upsert,
    /// Delete the node id from the external backend.
    Delete,
}

impl VectorSyncOperation {
    /// Returns the stable database representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Upsert => "upsert",
            Self::Delete => "delete",
        }
    }

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "upsert" => Ok(Self::Upsert),
            "delete" => Ok(Self::Delete),
            other => Err(Error::InvalidVectorSyncOperation(other.to_string())),
        }
    }
}

/// Summary of one vector outbox drain attempt.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorSyncReport {
    /// Rows claimed for processing.
    pub attempted: u64,
    /// Rows successfully applied to an external backend.
    pub succeeded: u64,
    /// Rows skipped because no external backend is configured for the partition.
    pub skipped: u64,
    /// Rows left pending after a processing failure.
    pub failed: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct VectorSyncJob {
    pub sync_id: i64,
    pub claim_token: Uuid,
    pub storage_partition_id: String,
    pub uid: Uuid,
    pub operation: VectorSyncOperation,
}

/// Enqueues external vector sync rows for a committed pgvector operation.
pub(crate) async fn enqueue_external_vector_sync(
    conn: &mut PgConnection,
    storage_partition_id: &str,
    operation: VectorSyncOperation,
    uids: &[Uuid],
) -> Result<()> {
    if uids.is_empty() {
        return Ok(());
    }

    sqlx::query(
        r#"
        INSERT INTO moa.vector_sync_outbox (storage_partition_id, uid, op)
        SELECT $1, queued.uid, $2
          FROM unnest($3::uuid[]) AS queued(uid)
         WHERE EXISTS (
               SELECT 1
                 FROM moa.storage_partition_state
                WHERE storage_partition_id = $1
                  AND vector_backend <> 'pgvector'
         )
        "#,
    )
    .bind(storage_partition_id)
    .bind(operation.as_str())
    .bind(uids)
    .execute(conn)
    .await?;
    Ok(())
}

/// Claims pending vector sync jobs for external processing.
pub(crate) async fn claim_pending_vector_sync(
    pool: &PgPool,
    storage_partition_id: Option<&str>,
    limit: i64,
) -> Result<Vec<VectorSyncJob>> {
    if limit <= 0 {
        return Ok(Vec::new());
    }

    let rows = sqlx::query(
        r#"
        WITH selected AS (
            SELECT sync_id
              FROM moa.vector_sync_outbox
             WHERE processed_at IS NULL
               AND available_at <= now()
               AND (claim_expires_at IS NULL OR claim_expires_at <= now())
               AND ($2::text IS NULL OR storage_partition_id = $2)
             ORDER BY sync_id
             LIMIT $1
             FOR UPDATE SKIP LOCKED
        )
        UPDATE moa.vector_sync_outbox AS outbox
           SET attempts = outbox.attempts + 1,
               claim_token = gen_random_uuid(),
               claim_expires_at = now() + INTERVAL '5 minutes',
               processing_started_at = now(),
               updated_at = now()
          FROM selected
         WHERE outbox.sync_id = selected.sync_id
        RETURNING outbox.sync_id,
                  outbox.claim_token,
                  outbox.storage_partition_id,
                  outbox.uid,
                  outbox.op
        "#,
    )
    .bind(limit)
    .bind(storage_partition_id)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            let operation: String = row.try_get("op")?;
            Ok(VectorSyncJob {
                sync_id: row.try_get("sync_id")?,
                claim_token: row.try_get("claim_token")?,
                storage_partition_id: row.try_get("storage_partition_id")?,
                uid: row.try_get("uid")?,
                operation: VectorSyncOperation::from_str(&operation)?,
            })
        })
        .collect()
}

/// Marks claimed vector sync jobs as processed.
pub(crate) async fn mark_vector_sync_processed_batch(
    pool: &PgPool,
    jobs: &[&VectorSyncJob],
) -> Result<()> {
    if jobs.is_empty() {
        return Ok(());
    }
    let (sync_ids, claim_tokens) = claim_pairs(jobs);
    sqlx::query(
        r#"
        WITH claimed(sync_id, claim_token) AS (
            SELECT *
              FROM unnest($1::bigint[], $2::uuid[])
        )
        UPDATE moa.vector_sync_outbox AS outbox
           SET processed_at = now(),
               claim_expires_at = NULL,
               updated_at = now(),
               last_error = NULL
          FROM claimed
         WHERE outbox.sync_id = claimed.sync_id
           AND outbox.claim_token = claimed.claim_token
           AND outbox.processed_at IS NULL
        "#,
    )
    .bind(sync_ids)
    .bind(claim_tokens)
    .execute(pool)
    .await?;
    Ok(())
}

/// Marks claimed vector sync jobs as failed and available for a later retry.
pub(crate) async fn mark_vector_sync_failed_batch(
    pool: &PgPool,
    jobs: &[&VectorSyncJob],
    error: &Error,
) -> Result<()> {
    if jobs.is_empty() {
        return Ok(());
    }
    let (sync_ids, claim_tokens) = claim_pairs(jobs);
    sqlx::query(
        r#"
        WITH claimed(sync_id, claim_token) AS (
            SELECT *
              FROM unnest($1::bigint[], $2::uuid[])
        )
        UPDATE moa.vector_sync_outbox AS outbox
           SET last_error = left($3, 2048),
               claim_expires_at = NULL,
               available_at = now() + INTERVAL '30 seconds',
               updated_at = now()
          FROM claimed
         WHERE outbox.sync_id = claimed.sync_id
           AND outbox.claim_token = claimed.claim_token
           AND outbox.processed_at IS NULL
        "#,
    )
    .bind(sync_ids)
    .bind(claim_tokens)
    .bind(error.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

/// Loads current pgvector source rows for queued upserts.
pub(crate) async fn fetch_current_vector_items(
    pool: &PgPool,
    storage_partition_id: &str,
    uids: &[Uuid],
) -> Result<Vec<VectorItem>> {
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        r#"
        SELECT embedding.uid,
               embedding.user_id,
               embedding.label,
               embedding.pii_class,
               embedding.embedding,
               embedding.embedding_model,
               embedding.embedding_model_version,
               knowledge_chunk.text AS search_text,
               embedding.valid_to
          FROM moa.embeddings AS embedding
          LEFT JOIN moa.knowledge_chunks AS knowledge_chunk
            ON knowledge_chunk.storage_partition_id = embedding.storage_partition_id
           AND knowledge_chunk.graph_node_uid = embedding.uid
           AND embedding.label = 'Chunk'
         WHERE embedding.storage_partition_id = $1
           AND embedding.uid = ANY($2::uuid[])
         ORDER BY embedding.uid
        "#,
    )
    .bind(storage_partition_id)
    .bind(uids)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(EmbeddingRow::from_row)
        .map(|row| row.and_then(|row| row.to_vector_item()))
        .collect()
}

fn claim_pairs(jobs: &[&VectorSyncJob]) -> (Vec<i64>, Vec<Uuid>) {
    let sync_ids = jobs.iter().map(|job| job.sync_id).collect();
    let claim_tokens = jobs.iter().map(|job| job.claim_token).collect();
    (sync_ids, claim_tokens)
}

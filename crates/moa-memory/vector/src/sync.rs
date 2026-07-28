//! Durable vector-backend sync queue for external vector projections.

use moa_db::ScopedConn;
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool, Postgres, Row, pool::PoolConnection};
use uuid::Uuid;

use crate::{Error, Result, VectorItem, embedding_row::EmbeddingRow};

/// Maximum outbox rows drained after a graph write commits.
pub const VECTOR_SYNC_POST_COMMIT_LIMIT: i64 = 64;

/// Transient-retry ceiling before a vector-sync row is quarantined (dead-lettered).
pub const VECTOR_SYNC_MAX_ATTEMPTS: i32 = 10;

/// Base delay, in seconds, for the exponential transient-retry backoff.
const VECTOR_SYNC_BACKOFF_BASE_SECS: f64 = 30.0;

/// Maximum delay, in seconds, that the transient-retry backoff can reach.
const VECTOR_SYNC_BACKOFF_CAP_SECS: f64 = 3600.0;

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
    /// Rows quarantined (dead-lettered) after a permanent or exhausted failure.
    pub dead_lettered: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct VectorSyncJob {
    pub sync_id: i64,
    pub claim_token: Uuid,
    pub storage_partition_id: String,
    pub uid: Uuid,
    pub operation: VectorSyncOperation,
}

/// Dedicated session lock held across one partition's remote vector I/O and claim settlement.
pub(crate) struct VectorSyncRemoteGuard {
    conn: PoolConnection<Postgres>,
    tenant_id: Uuid,
    fenced: bool,
}

impl VectorSyncRemoteGuard {
    /// Returns whether tenant destruction began before this remote-I/O lock was acquired.
    pub(crate) fn is_fenced(&self) -> bool {
        self.fenced
    }

    /// Releases the session advisory lock after every claimed job has settled.
    pub(crate) async fn finish(mut self) -> Result<()> {
        sqlx::query(
            "SELECT pg_advisory_unlock(hashtextextended('moa:destruction:tenant:' || $1::text, 0))",
        )
        .bind(self.tenant_id)
        .execute(&mut *self.conn)
        .await?;
        Ok(())
    }
}

/// Enqueues external vector sync rows for a committed pgvector operation.
pub async fn enqueue_external_vector_sync(
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

    let mut conn = ScopedConn::begin_control_plane(pool).await?;
    let tenant_ids = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT DISTINCT partition.tenant_id
          FROM moa.vector_sync_outbox AS outbox
          JOIN moa.storage_partition_state AS partition
            ON partition.storage_partition_id = outbox.storage_partition_id
         WHERE outbox.processed_at IS NULL
           AND outbox.dead_lettered_at IS NULL
           AND outbox.available_at <= now()
           AND (outbox.claim_expires_at IS NULL OR outbox.claim_expires_at <= now())
           AND ($2::text IS NULL OR outbox.storage_partition_id = $2)
           AND NOT EXISTS (
               SELECT 1
                 FROM moa.destruction_operation_fence AS fence
                WHERE fence.tenant_id = partition.tenant_id
                  AND fence.subject_id IS NULL
                  AND fence.status IN ('in_progress', 'committed')
           )
         ORDER BY partition.tenant_id
         LIMIT $1
        "#,
    )
    .bind(limit)
    .bind(storage_partition_id)
    .fetch_all(conn.as_mut())
    .await?;

    for tenant_id in &tenant_ids {
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended('moa:destruction:tenant:' || $1::text, 0))",
        )
        .bind(tenant_id)
        .execute(conn.as_mut())
        .await?;
    }

    let rows = sqlx::query(
        r#"
        WITH selected AS (
            SELECT outbox.sync_id
              FROM moa.vector_sync_outbox AS outbox
              JOIN moa.storage_partition_state AS partition
                ON partition.storage_partition_id = outbox.storage_partition_id
             WHERE outbox.processed_at IS NULL
               AND outbox.dead_lettered_at IS NULL
               AND outbox.available_at <= now()
               AND (outbox.claim_expires_at IS NULL OR outbox.claim_expires_at <= now())
               AND ($2::text IS NULL OR outbox.storage_partition_id = $2)
               AND partition.tenant_id = ANY($3::uuid[])
               AND NOT EXISTS (
                   SELECT 1
                     FROM moa.destruction_operation_fence AS fence
                    WHERE fence.tenant_id = partition.tenant_id
                      AND fence.subject_id IS NULL
                      AND fence.status IN ('in_progress', 'committed')
               )
             ORDER BY outbox.sync_id
             LIMIT $1
             FOR UPDATE OF outbox SKIP LOCKED
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
    .bind(&tenant_ids)
    .fetch_all(conn.as_mut())
    .await?;
    conn.commit().await?;

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

/// Acquires the tenant destruction boundary across remote vector I/O.
///
/// The dedicated connection is closed instead of returned to the pool on every
/// exit, so cancellation cannot leak a session advisory lock to another pool
/// borrower. A missing partition-state row returns `None`; without that durable
/// routing state there is no configured external backend to call.
pub(crate) async fn begin_vector_sync_remote_guard(
    pool: &PgPool,
    storage_partition_id: &str,
) -> Result<Option<VectorSyncRemoteGuard>> {
    let mut conn = pool.acquire().await?;
    conn.close_on_drop();
    sqlx::query(
        r#"
        SELECT pg_catalog.set_config('moa.tenant_id', '', false),
               pg_catalog.set_config('moa.storage_partition_id', $1, false),
               pg_catalog.set_config('moa.control_plane', 'true', false)
        "#,
    )
    .bind(storage_partition_id)
    .execute(&mut *conn)
    .await?;

    let Some(tenant_id) = sqlx::query_scalar::<_, Uuid>(
        "SELECT tenant_id FROM moa.storage_partition_state WHERE storage_partition_id = $1",
    )
    .bind(storage_partition_id)
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(None);
    };

    sqlx::query(
        "SELECT pg_advisory_lock(hashtextextended('moa:destruction:tenant:' || $1::text, 0))",
    )
    .bind(tenant_id)
    .execute(&mut *conn)
    .await?;
    let fenced = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
              FROM moa.destruction_operation_fence
             WHERE tenant_id = $1
               AND subject_id IS NULL
               AND status IN ('in_progress', 'committed')
        )
        "#,
    )
    .bind(tenant_id)
    .fetch_one(&mut *conn)
    .await?;

    Ok(Some(VectorSyncRemoteGuard {
        conn,
        tenant_id,
        fenced,
    }))
}

/// Returns whether a partition has a live vector-sync claim that purge must wait for.
pub async fn has_active_vector_sync_claims(
    pool: &PgPool,
    storage_partition_id: &str,
) -> Result<bool> {
    let mut conn = ScopedConn::begin_control_plane(pool).await?;
    sqlx::query("SELECT pg_catalog.set_config('moa.storage_partition_id', $1, true)")
        .bind(storage_partition_id)
        .execute(conn.as_mut())
        .await?;
    let active = has_active_vector_sync_claims_in_tx(conn.as_mut(), storage_partition_id).await?;
    conn.commit().await?;
    Ok(active)
}

/// Returns whether a partition has a live vector-sync claim in the caller's transaction.
///
/// A claim with no expiry is treated as active so malformed durable state fails
/// closed instead of allowing tenant purge to race an unbounded worker.
pub async fn has_active_vector_sync_claims_in_tx(
    conn: &mut PgConnection,
    storage_partition_id: &str,
) -> Result<bool> {
    Ok(sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
              FROM moa.vector_sync_outbox
             WHERE storage_partition_id = $1
               AND processed_at IS NULL
               AND claim_token IS NOT NULL
               AND (claim_expires_at IS NULL OR claim_expires_at > now())
        )
        "#,
    )
    .bind(storage_partition_id)
    .fetch_one(conn)
    .await?)
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

/// Marks claimed vector sync jobs as failed, returning how many were quarantined.
///
/// A `permanent` failure (or a transient one that has exhausted
/// [`VECTOR_SYNC_MAX_ATTEMPTS`]) dead-letters the row: `dead_lettered_at` is set
/// and the claim predicate no longer selects it. A recoverable transient failure
/// instead schedules the next attempt with exponential backoff derived from the
/// row's `attempts`, capped at [`VECTOR_SYNC_BACKOFF_CAP_SECS`]. The returned
/// count is the number of rows that transitioned to the dead-letter state.
pub(crate) async fn mark_vector_sync_failed_batch(
    pool: &PgPool,
    jobs: &[&VectorSyncJob],
    error: &Error,
    permanent: bool,
) -> Result<u64> {
    if jobs.is_empty() {
        return Ok(0);
    }
    let (sync_ids, claim_tokens) = claim_pairs(jobs);
    let rows = sqlx::query(
        r#"
        WITH claimed(sync_id, claim_token) AS (
            SELECT *
              FROM unnest($1::bigint[], $2::uuid[])
        )
        UPDATE moa.vector_sync_outbox AS outbox
           SET last_error = left($3, 2048),
               claim_token = NULL,
               claim_expires_at = NULL,
               updated_at = now(),
               dead_lettered_at = CASE
                   WHEN $4 OR outbox.attempts >= $5 THEN now()
                   ELSE outbox.dead_lettered_at
               END,
               available_at = CASE
                   WHEN $4 OR outbox.attempts >= $5 THEN outbox.available_at
                   ELSE now() + make_interval(
                       secs => LEAST($6, $7 * power(2, GREATEST(outbox.attempts - 1, 0)))
                   )
               END
          FROM claimed
         WHERE outbox.sync_id = claimed.sync_id
           AND outbox.claim_token = claimed.claim_token
           AND outbox.processed_at IS NULL
        RETURNING (outbox.dead_lettered_at IS NOT NULL) AS dead_lettered
        "#,
    )
    .bind(sync_ids)
    .bind(claim_tokens)
    .bind(error.to_string())
    .bind(permanent)
    .bind(VECTOR_SYNC_MAX_ATTEMPTS)
    .bind(VECTOR_SYNC_BACKOFF_CAP_SECS)
    .bind(VECTOR_SYNC_BACKOFF_BASE_SECS)
    .fetch_all(pool)
    .await?;

    let dead_lettered = rows
        .iter()
        .filter(|row| row.get::<bool, _>("dead_lettered"))
        .count() as u64;
    Ok(dead_lettered)
}

/// Resets quarantined (dead-lettered) vector-sync rows to pending for redrive.
///
/// Intended for an operator action after the permanent fault has been remediated.
/// Clears `dead_lettered_at`, resets the attempt counter and claim, and makes the
/// rows immediately eligible. Returns the number of rows re-queued.
pub(crate) async fn redrive_dead_lettered_vector_sync(
    pool: &PgPool,
    storage_partition_id: Option<&str>,
) -> Result<u64> {
    let result = sqlx::query(
        r#"
        UPDATE moa.vector_sync_outbox
           SET dead_lettered_at = NULL,
               attempts = 0,
               available_at = now(),
               last_error = NULL,
               claim_token = NULL,
               claim_expires_at = NULL,
               updated_at = now()
         WHERE dead_lettered_at IS NOT NULL
           AND processed_at IS NULL
           AND ($1::text IS NULL OR storage_partition_id = $1)
        "#,
    )
    .bind(storage_partition_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
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

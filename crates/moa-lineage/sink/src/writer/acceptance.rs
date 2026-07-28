//! Postgres-owned lineage acceptance: commit, claim, lease, and dequeue.
//!
//! This module is the only place that decides what "accepted" means. A batch is
//! accepted when [`LineageJournal::accept_batch`] returns, and it returns only
//! after `COMMIT`. Every other component in the writer treats a journal row as
//! the authoritative copy of a record, which is why no path here removes a row
//! without either storing its content or dead-lettering it in the same
//! transaction.
//!
//! The pod-local journal this replaces returned "accepted" after an fsync to a
//! directory only one replica could see, so a rollout destroyed records their
//! callers had already been told were durable.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, QueryBuilder, Row, Transaction};
use uuid::Uuid;

use crate::{Error, Result};

use super::rows::{PendingRow, decode_pending_row, pending_row_event_class};

/// One row claimed from the queue, with the payload still undecoded.
#[derive(Debug, Clone)]
pub(super) struct ClaimedRow {
    /// Queue identity, used to dequeue the row after its content is stored.
    pub(super) journal_id: Uuid,
    /// Serialized pending row.
    pub(super) payload: serde_json::Value,
    /// Number of claims this row has had, including the current one.
    pub(super) attempts: i32,
}

/// Backlog health as the queue itself reports it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct JournalBacklog {
    /// Rows accepted and not yet stored, whatever their lease state.
    pub pending: u64,
    /// Age in seconds of the oldest accepted-but-unstored row.
    ///
    /// Deliberately unfiltered by lease or backoff state. Restricting it to
    /// currently *claimable* rows reads better on a healthy fleet, but it hides
    /// exactly the backlog that matters: a row deferred into the future by a
    /// repeated write failure is not claimable, and would report an empty
    /// backlog while the record it holds gets older forever.
    pub oldest_pending_age_seconds: Option<f64>,
}

/// Durable acceptance queue backed by `analytics.lineage_journal`.
#[derive(Clone)]
pub struct LineageJournal {
    pool: PgPool,
    owner: Uuid,
    lease_ttl: Duration,
}

impl LineageJournal {
    /// Binds the queue to a pool, with this process as the lease owner.
    ///
    /// The owner id is per-instance rather than per-host: two writers in one
    /// process would otherwise be able to steal each other's live leases.
    #[must_use]
    pub fn new(pool: PgPool, lease_ttl: Duration) -> Self {
        Self {
            pool,
            owner: Uuid::now_v7(),
            lease_ttl,
        }
    }

    /// Commits a whole batch of pending rows and returns their queue ids.
    ///
    /// Returning means committed. Callers may treat the batch as durable from
    /// this point on, and any replica can finish it.
    pub(crate) async fn accept_batch(&self, rows: &[PendingRow]) -> Result<Vec<Uuid>> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }

        let mut prepared = Vec::with_capacity(rows.len());
        for row in rows {
            let journal_id = Uuid::now_v7();
            let payload = serde_json::to_value(row)?;
            prepared.push((
                journal_id,
                row.storage_partition_id(),
                row.user_id(),
                pending_row_event_class(row),
                payload,
            ));
        }

        let mut tx = self.begin().await?;
        let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
            "INSERT INTO analytics.lineage_journal \
             (journal_id, storage_partition_id, user_id, event_class, payload) ",
        );
        builder.push_values(
            &prepared,
            |mut insert, (journal_id, partition, user_id, event_class, payload)| {
                insert
                    .push_bind(*journal_id)
                    .push_bind(partition)
                    .push_bind(user_id)
                    .push_bind(*event_class)
                    .push_bind(payload);
            },
        );
        builder.build().execute(&mut *tx).await?;
        tx.commit().await?;

        metrics::counter!("moa_lineage_accepted_total", "durability" => "postgres")
            .increment(prepared.len() as u64);
        Ok(prepared
            .into_iter()
            .map(|(journal_id, ..)| journal_id)
            .collect())
    }

    /// Claims up to `limit` eligible rows under an expiring lease.
    ///
    /// Rows are taken in acceptance order through the claim index with
    /// `FOR UPDATE SKIP LOCKED`, so concurrent replicas take disjoint work and
    /// never block on each other. The lease is what makes a claimant's death
    /// self-healing: nothing has to notice the death, the lease simply expires
    /// and the rows become eligible again.
    pub(super) async fn claim_batch(&self, limit: usize) -> Result<Vec<ClaimedRow>> {
        let limit = i64::try_from(limit)
            .map_err(|_| Error::Invalid("lineage claim batch size overflow".to_string()))?;
        let lease_ms = i64::try_from(self.lease_ttl.as_millis())
            .map_err(|_| Error::Invalid("lineage lease ttl overflow".to_string()))?;

        let mut tx = self.begin().await?;
        let rows = sqlx::query(
            r#"
            WITH claimed AS (
                SELECT journal_id
                FROM analytics.lineage_journal
                WHERE claimable_at <= now()
                ORDER BY claimable_at, journal_id
                LIMIT $1
                FOR UPDATE SKIP LOCKED
            )
            UPDATE analytics.lineage_journal AS journal
            SET lease_owner = $2,
                lease_expires_at = now() + make_interval(secs => $3 / 1000.0),
                attempts = journal.attempts + 1
            FROM claimed
            WHERE journal.journal_id = claimed.journal_id
            RETURNING journal.journal_id, journal.payload, journal.attempts
            "#,
        )
        .bind(limit)
        .bind(self.owner)
        .bind(lease_ms)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;

        let claimed = rows
            .into_iter()
            .map(|row| ClaimedRow {
                journal_id: row.get("journal_id"),
                payload: row.get("payload"),
                attempts: row.get("attempts"),
            })
            .collect::<Vec<_>>();
        if !claimed.is_empty() {
            metrics::counter!("moa_lineage_journal_claimed_total").increment(claimed.len() as u64);
        }
        Ok(claimed)
    }

    /// Begins a transaction scoped to the internal control plane.
    ///
    /// The queue's row-level security admits only the control plane. Setting the
    /// flag here rather than at the caller means there is no journal statement
    /// that can accidentally run outside it — a statement outside it would not
    /// fail loudly, it would silently see an empty queue.
    pub(super) async fn begin(&self) -> Result<Transaction<'_, Postgres>> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT set_config('moa.control_plane', 'true', true)")
            .execute(&mut *tx)
            .await?;
        Ok(tx)
    }

    /// Removes stored rows from the queue inside the caller's transaction.
    ///
    /// This is deliberately not a standalone method that opens its own
    /// transaction: dequeue is only ever correct when it commits with the write
    /// that stored the content.
    pub(super) async fn dequeue_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        journal_ids: &[Uuid],
    ) -> Result<u64> {
        if journal_ids.is_empty() {
            return Ok(0);
        }
        let deleted =
            sqlx::query("DELETE FROM analytics.lineage_journal WHERE journal_id = ANY($1)")
                .bind(journal_ids)
                .execute(&mut **tx)
                .await?
                .rows_affected();
        Ok(deleted)
    }

    /// Releases a lease and defers the rows so a recoverable failure retries.
    ///
    /// The rows are preserved: a recoverable failure must never consume a
    /// record. `attempts` is left as claimed, so the retry budget still shrinks.
    pub(super) async fn defer_claim(&self, journal_ids: &[Uuid], backoff: Duration) -> Result<()> {
        if journal_ids.is_empty() {
            return Ok(());
        }
        let backoff_ms = i64::try_from(backoff.as_millis())
            .map_err(|_| Error::Invalid("lineage retry backoff overflow".to_string()))?;
        let mut tx = self.begin().await?;
        sqlx::query(
            r#"
            UPDATE analytics.lineage_journal
            SET lease_owner = NULL,
                lease_expires_at = NULL,
                available_at = now() + make_interval(secs => $2 / 1000.0)
            WHERE journal_id = ANY($1)
            "#,
        )
        .bind(journal_ids)
        .bind(backoff_ms)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        metrics::counter!("moa_lineage_journal_deferred_total").increment(journal_ids.len() as u64);
        Ok(())
    }

    /// Reads backlog depth and the age of the oldest claimable row.
    pub(super) async fn backlog(&self) -> Result<JournalBacklog> {
        let mut tx = self.begin().await?;
        let row = sqlx::query(
            r#"
            SELECT
                count(*) AS pending,
                min(accepted_at) AS oldest_pending
            FROM analytics.lineage_journal
            "#,
        )
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;

        let pending: i64 = row.get("pending");
        let oldest: Option<DateTime<Utc>> = row.get("oldest_pending");
        Ok(JournalBacklog {
            pending: pending.max(0) as u64,
            oldest_pending_age_seconds: oldest.map(|accepted_at| {
                ((Utc::now() - accepted_at).num_milliseconds().max(0) as f64) / 1000.0
            }),
        })
    }
}

/// Decodes a claimed payload, mapping a decode failure into a permanent error.
///
/// A payload that will not decode can never be written, so it is not retryable
/// under any backoff. It is a dead-letter candidate at the first attempt.
pub(super) fn decode_claimed(claimed: &ClaimedRow) -> Result<PendingRow> {
    decode_pending_row(claimed.payload.clone()).map_err(|error| {
        Error::Invalid(format!(
            "undecodable lineage journal payload {}: {error}",
            claimed.journal_id
        ))
    })
}

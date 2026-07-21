//! Background worker that drains `authz_outbox` rows into OpenFGA.

use crate::client::FgaClient;
use crate::error::AuthzError;
use crate::outbox::OutboxRow;
use serde_json::json;
use sqlx::PgPool;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::sleep;
use uuid::Uuid;

/// Tunable outbox-poller settings.
#[derive(Debug, Clone)]
pub struct PollerConfig {
    /// Maximum rows claimed in one poller tick.
    pub batch_size: usize,
    /// Delay between poller ticks.
    pub poll_interval: Duration,
    /// Maximum attempts before a row is moved to `dead_letter`.
    pub max_attempts: i32,
    /// First retry delay.
    pub backoff_base: Duration,
    /// Maximum retry delay.
    pub backoff_cap: Duration,
    /// How long one poller owns an `in_flight` row before another worker may reclaim it.
    pub lease_duration: Duration,
}

impl Default for PollerConfig {
    fn default() -> Self {
        Self {
            batch_size: 64,
            poll_interval: Duration::from_millis(500),
            max_attempts: 8,
            backoff_base: Duration::from_millis(200),
            backoff_cap: Duration::from_secs(60),
            lease_duration: Duration::from_secs(300),
        }
    }
}

/// Background poller that claims pending outbox rows and applies them to OpenFGA.
pub struct OutboxPoller {
    pool: PgPool,
    client: FgaClient,
    cfg: PollerConfig,
}

impl OutboxPoller {
    /// Build a poller over an existing Postgres pool and OpenFGA client.
    #[must_use]
    pub fn new(pool: PgPool, client: FgaClient, cfg: PollerConfig) -> Self {
        Self { pool, client, cfg }
    }

    /// Spawn the poller on the current Tokio runtime and return a shutdown handle.
    pub fn spawn(self) -> PollerHandle {
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        tracing::info!("outbox poller received shutdown");
                        break;
                    }
                    result = self.tick() => {
                        if let Err(error) = result {
                            tracing::error!(error = %error, "outbox poller tick failed");
                        }
                    }
                }

                tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        tracing::info!("outbox poller received shutdown");
                        break;
                    }
                    _ = sleep(self.cfg.poll_interval) => {}
                }
            }
        });

        PollerHandle {
            shutdown: Some(shutdown_tx),
            task,
        }
    }

    /// Run one drain pass and return the number of rows applied successfully.
    pub async fn tick(&self) -> Result<usize, AuthzError> {
        // Sample the backlog age every tick (including idle ticks) so a stalled
        // drain surfaces as a rising gauge rather than only in logs.
        self.record_backlog_gauge().await;
        let claimed = self.claim_batch().await?;
        if claimed.is_empty() {
            return Ok(0);
        }

        let mut applied = 0usize;
        for claim in claimed {
            match self.apply_row(&claim.row).await {
                Ok(()) => {
                    if self.record_success(&claim).await? {
                        applied += 1;
                    }
                }
                Err(error) => {
                    self.record_failure(&claim, &error).await?;
                }
            }
        }

        Ok(applied)
    }

    /// Delivers the current desired state for one exact OpenFGA object.
    ///
    /// This is the synchronous visibility barrier used after committing a
    /// product row and its authorization tuples. It snapshots exact row and
    /// generation receipts, accepts already-succeeded receipts, and may take
    /// over a pending or in-flight receipt from another replica. Dead letters
    /// are never revived. OpenFGA tuple operations are idempotent and the
    /// generation-fenced success update ensures only the snapshotted desired
    /// state is completed. No process-local notification or poll interval
    /// participates in correctness.
    pub async fn flush_object(&self, object: &str) -> Result<usize, AuthzError> {
        let receipts: Vec<OutboxReceiptState> = sqlx::query_as(
            r#"
            SELECT id, generation, status
            FROM authz_outbox
            WHERE tuple_object = $1
            ORDER BY id
            "#,
        )
        .bind(object)
        .fetch_all(&self.pool)
        .await?;
        if receipts.is_empty() {
            return Ok(0);
        }

        let mut satisfied = 0usize;
        for receipt in receipts {
            if receipt.status == "succeeded" {
                satisfied += 1;
                continue;
            }
            if receipt.status == "dead_letter" {
                return Err(AuthzError::Ambiguous(format!(
                    "authorization receipt {} generation {} is dead-lettered",
                    receipt.id, receipt.generation
                )));
            }
            let Some(claim) = self.claim_receipt(&receipt).await? else {
                if missed_claim_is_satisfied(self.receipt_status(&receipt).await?.as_deref()) {
                    satisfied += 1;
                    continue;
                }
                return Err(AuthzError::Ambiguous(format!(
                    "authorization receipt {} generation {} changed while flushing {object}",
                    receipt.id, receipt.generation
                )));
            };
            if let Err(error) = self.apply_row(&claim.row).await {
                self.record_failure(&claim, &error).await?;
                return Err(error);
            }
            if !self.record_success(&claim).await? {
                if missed_claim_is_satisfied(self.receipt_status(&receipt).await?.as_deref()) {
                    satisfied += 1;
                    continue;
                }
                return Err(AuthzError::Ambiguous(format!(
                    "authorization tuple changed while flushing {object}"
                )));
            }
            satisfied += 1;
        }
        Ok(satisfied)
    }

    async fn claim_batch(&self) -> Result<Vec<ClaimedOutboxRow>, AuthzError> {
        let lease_token = Uuid::new_v4();
        let claimed: Vec<ClaimedOutboxRecord> = sqlx::query_as(
            r#"
            WITH candidate AS (
                SELECT id
                FROM authz_outbox
                WHERE (
                        status = 'pending'
                        AND next_attempt_at <= NOW()
                    )
                    OR (
                        status = 'in_flight'
                        AND (
                            lease_expires_at IS NULL
                            OR lease_expires_at <= NOW()
                        )
                    )
                ORDER BY next_attempt_at, updated_at
                LIMIT $1
                FOR UPDATE SKIP LOCKED
            )
            UPDATE authz_outbox AS outbox
            SET status = 'in_flight',
                lease_token = $2,
                lease_expires_at = NOW() + ($3 || ' milliseconds')::INTERVAL,
                updated_at = NOW()
            FROM candidate
            WHERE outbox.id = candidate.id
            RETURNING outbox.id, outbox.op,
                      outbox.tuple_user, outbox.tuple_relation, outbox.tuple_object,
                      outbox.attempts, outbox.generation, outbox.lease_token
            "#,
        )
        .bind(limit_i64(self.cfg.batch_size))
        .bind(lease_token)
        .bind(duration_millis_string(self.cfg.lease_duration))
        .fetch_all(&self.pool)
        .await?;
        Ok(claimed.into_iter().map(ClaimedOutboxRow::from).collect())
    }

    async fn claim_receipt(
        &self,
        receipt: &OutboxReceiptState,
    ) -> Result<Option<ClaimedOutboxRow>, AuthzError> {
        let lease_token = Uuid::new_v4();
        let claimed: Option<ClaimedOutboxRecord> = sqlx::query_as(
            r#"
            UPDATE authz_outbox
            SET status = 'in_flight',
                lease_token = $2,
                lease_expires_at = NOW() + ($3 || ' milliseconds')::INTERVAL,
                updated_at = NOW()
            WHERE id = $1
              AND generation = $4
              AND status IN ('pending', 'in_flight')
            RETURNING id, op, tuple_user, tuple_relation, tuple_object,
                      attempts, generation, lease_token
            "#,
        )
        .bind(receipt.id)
        .bind(lease_token)
        .bind(duration_millis_string(self.cfg.lease_duration))
        .bind(receipt.generation)
        .fetch_optional(&self.pool)
        .await?;
        Ok(claimed.map(ClaimedOutboxRow::from))
    }

    async fn receipt_status(
        &self,
        receipt: &OutboxReceiptState,
    ) -> Result<Option<String>, AuthzError> {
        sqlx::query_scalar("SELECT status FROM authz_outbox WHERE id = $1 AND generation = $2")
            .bind(receipt.id)
            .bind(receipt.generation)
            .fetch_optional(&self.pool)
            .await
            .map_err(AuthzError::from)
    }

    async fn apply_row(&self, row: &OutboxRow) -> Result<(), AuthzError> {
        let wire = json!({
            "user": &row.tuple_user,
            "relation": &row.tuple_relation,
            "object": &row.tuple_object,
        });
        let body = match row.op.as_str() {
            "write" => json!({
                "authorization_model_id": self.client.model_id(),
                "writes": { "tuple_keys": [wire] },
            }),
            "delete" => json!({
                "authorization_model_id": self.client.model_id(),
                "deletes": { "tuple_keys": [wire] },
            }),
            other => return Err(AuthzError::Ambiguous(format!("unknown outbox op: {other}"))),
        };
        self.client.apply_raw(body).await
    }

    async fn record_success(&self, claim: &ClaimedOutboxRow) -> Result<bool, AuthzError> {
        // Compare-and-set on generation: only mark succeeded if the desired state
        // this poller applied is still current. A concurrent enqueue that changed
        // the desired op reset the row to pending and bumped its generation, so
        // this update matches zero rows and the newer state is applied next tick.
        let result = sqlx::query(
            r#"
            UPDATE authz_outbox
            SET status='succeeded',
                lease_token = NULL,
                lease_expires_at = NULL,
                updated_at=NOW()
            WHERE id=$1
              AND status = 'in_flight'
              AND lease_token = $2
              AND generation = $3
            "#,
        )
        .bind(claim.row.id)
        .bind(claim.lease_token)
        .bind(claim.row.generation)
        .execute(&self.pool)
        .await?;
        let completed = result.rows_affected() == 1;
        if !completed {
            tracing::debug!(
                id = %claim.row.id,
                tuple = %claim.tuple_display(),
                generation = claim.row.generation,
                "outbox row desired state changed before success could be recorded"
            );
        }
        Ok(completed)
    }

    async fn record_failure(
        &self,
        claim: &ClaimedOutboxRow,
        error: &AuthzError,
    ) -> Result<(), AuthzError> {
        let row = &claim.row;
        let next_attempts = row.attempts + 1;
        // Every failure update is fenced on generation as well as the lease, so a
        // concurrent desired-state change (which reactivated the row at a higher
        // generation) is never overwritten with stale retry/dead-letter state.
        if next_attempts >= self.cfg.max_attempts {
            let result = sqlx::query(
                r#"
                UPDATE authz_outbox
                SET status='dead_letter',
                    attempts=$2,
                    last_error=$3,
                    lease_token = NULL,
                    lease_expires_at = NULL,
                    updated_at=NOW()
                WHERE id=$1
                  AND status = 'in_flight'
                  AND lease_token = $4
                  AND generation = $5
                "#,
            )
            .bind(row.id)
            .bind(next_attempts)
            .bind(error.to_string())
            .bind(claim.lease_token)
            .bind(row.generation)
            .execute(&self.pool)
            .await?;
            if result.rows_affected() == 1 {
                tracing::error!(
                    id = %row.id,
                    tuple = %claim.tuple_display(),
                    generation = row.generation,
                    attempts = next_attempts,
                    error = %error,
                    "outbox row exhausted retries; moving to dead_letter"
                );
                metrics::counter!("moa_authz_outbox_dead_letters_total").increment(1);
            } else {
                tracing::debug!(
                    id = %row.id,
                    generation = row.generation,
                    "outbox failure ignored after lease or generation changed"
                );
            }
            return Ok(());
        }

        let backoff = self.backoff_for(next_attempts);
        let result = sqlx::query(
            r#"
            UPDATE authz_outbox
            SET status='pending',
                attempts=$2,
                last_error=$3,
                next_attempt_at=NOW() + ($4 || ' milliseconds')::INTERVAL,
                lease_token = NULL,
                lease_expires_at = NULL,
                updated_at=NOW()
            WHERE id=$1
              AND status = 'in_flight'
              AND lease_token = $5
              AND generation = $6
            "#,
        )
        .bind(row.id)
        .bind(next_attempts)
        .bind(error.to_string())
        .bind(duration_millis_string(backoff))
        .bind(claim.lease_token)
        .bind(row.generation)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 1 {
            tracing::warn!(
                id = %row.id,
                tuple = %claim.tuple_display(),
                generation = row.generation,
                attempts = next_attempts,
                backoff_ms = backoff.as_millis() as u64,
                error = %error,
                "outbox row failed; backing off"
            );
            metrics::counter!("moa_authz_outbox_retries_total").increment(1);
        } else {
            tracing::debug!(
                id = %row.id,
                generation = row.generation,
                "outbox failure ignored after lease or generation changed"
            );
        }
        Ok(())
    }

    /// Samples the drain-backlog age gauge: how long the oldest ready-to-apply
    /// pending row has waited. Best-effort; a sampling error never fails the tick.
    async fn record_backlog_gauge(&self) {
        match self.oldest_ready_pending_age_seconds().await {
            Ok(age) => {
                metrics::gauge!("moa_authz_outbox_oldest_pending_age_seconds").set(age);
            }
            Err(error) => {
                tracing::debug!(error = %error, "failed to sample authz outbox backlog age");
            }
        }
    }

    /// Returns the age in seconds of the oldest pending row whose next attempt is
    /// due, or `0.0` when nothing is waiting to be applied.
    async fn oldest_ready_pending_age_seconds(&self) -> Result<f64, AuthzError> {
        let age: Option<f64> = sqlx::query_scalar(
            r#"
            SELECT EXTRACT(EPOCH FROM (NOW() - MIN(next_attempt_at)))
            FROM authz_outbox
            WHERE status = 'pending' AND next_attempt_at <= NOW()
            "#,
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(age.unwrap_or(0.0).max(0.0))
    }

    fn backoff_for(&self, attempt: i32) -> Duration {
        let pow = (attempt as u32).saturating_sub(1).min(20);
        let multiplier = 1u64 << pow;
        let millis = (self.cfg.backoff_base.as_millis() as u64).saturating_mul(multiplier);
        Duration::from_millis(millis).min(self.cfg.backoff_cap)
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct OutboxReceiptState {
    id: Uuid,
    generation: i64,
    status: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct ClaimedOutboxRecord {
    id: Uuid,
    op: String,
    tuple_user: String,
    tuple_relation: String,
    tuple_object: String,
    attempts: i32,
    generation: i64,
    lease_token: Uuid,
}

impl ClaimedOutboxRecord {
    fn row(&self) -> OutboxRow {
        OutboxRow {
            id: self.id,
            op: self.op.clone(),
            tuple_user: self.tuple_user.clone(),
            tuple_relation: self.tuple_relation.clone(),
            tuple_object: self.tuple_object.clone(),
            attempts: self.attempts,
            generation: self.generation,
        }
    }
}

impl From<ClaimedOutboxRecord> for ClaimedOutboxRow {
    fn from(claim: ClaimedOutboxRecord) -> Self {
        Self {
            row: claim.row(),
            lease_token: claim.lease_token,
        }
    }
}

#[derive(Debug, Clone)]
struct ClaimedOutboxRow {
    row: OutboxRow,
    lease_token: Uuid,
}

impl ClaimedOutboxRow {
    /// Render the tuple identity for structured logs, e.g. `write user rel object`.
    fn tuple_display(&self) -> String {
        format!(
            "{} {} {} {}",
            self.row.op, self.row.tuple_user, self.row.tuple_relation, self.row.tuple_object
        )
    }
}

/// Handle for cleanly shutting down a spawned outbox poller.
pub struct PollerHandle {
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl PollerHandle {
    /// Signal the poller to stop and wait for its task to exit.
    pub async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.task.await;
    }
}

fn limit_i64(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

fn duration_millis_string(duration: Duration) -> String {
    duration.as_millis().to_string()
}

fn missed_claim_is_satisfied(status: Option<&str>) -> bool {
    status == Some("succeeded")
}

#[cfg(test)]
mod tests {
    use super::missed_claim_is_satisfied;

    #[test]
    fn missed_claim_accepts_only_same_generation_success() {
        // Pins: after an exact `(id, generation)` claim loses a replica race,
        // only a re-read `succeeded` state satisfies the visibility barrier.
        assert!(missed_claim_is_satisfied(Some("succeeded")));
        for status in [
            None,
            Some("pending"),
            Some("in_flight"),
            Some("dead_letter"),
        ] {
            assert!(!missed_claim_is_satisfied(status), "status={status:?}");
        }
    }
}

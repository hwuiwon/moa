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

    async fn claim_batch(&self) -> Result<Vec<ClaimedOutboxRow>, AuthzError> {
        let lease_token = Uuid::new_v4();
        let mut tx = self.pool.begin().await?;
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
            RETURNING outbox.id, outbox.idempotency_key, outbox.op,
                      outbox.tuple_user, outbox.tuple_relation, outbox.tuple_object,
                      outbox.attempts, outbox.lease_token
            "#,
        )
        .bind(limit_i64(self.cfg.batch_size))
        .bind(lease_token)
        .bind(duration_millis_string(self.cfg.lease_duration))
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(claimed.into_iter().map(ClaimedOutboxRow::from).collect())
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
            "#,
        )
        .bind(claim.row.id)
        .bind(claim.lease_token)
        .execute(&self.pool)
        .await?;
        let completed = result.rows_affected() == 1;
        if !completed {
            tracing::debug!(
                id = %claim.row.id,
                key = %claim.row.idempotency_key,
                "outbox row lease was lost before success could be recorded"
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
        if next_attempts >= self.cfg.max_attempts {
            tracing::error!(
                id = %row.id,
                key = %row.idempotency_key,
                attempts = next_attempts,
                error = %error,
                "outbox row exhausted retries; moving to dead_letter"
            );
            sqlx::query(
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
                "#,
            )
            .bind(row.id)
            .bind(next_attempts)
            .bind(error.to_string())
            .bind(claim.lease_token)
            .execute(&self.pool)
            .await?;
            return Ok(());
        }

        let backoff = self.backoff_for(next_attempts);
        tracing::warn!(
            id = %row.id,
            key = %row.idempotency_key,
            attempts = next_attempts,
            backoff_ms = backoff.as_millis() as u64,
            error = %error,
            "outbox row failed; backing off"
        );
        sqlx::query(
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
            "#,
        )
        .bind(row.id)
        .bind(next_attempts)
        .bind(error.to_string())
        .bind(duration_millis_string(backoff))
        .bind(claim.lease_token)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    fn backoff_for(&self, attempt: i32) -> Duration {
        let pow = (attempt as u32).saturating_sub(1).min(20);
        let multiplier = 1u64 << pow;
        let millis = (self.cfg.backoff_base.as_millis() as u64).saturating_mul(multiplier);
        Duration::from_millis(millis).min(self.cfg.backoff_cap)
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct ClaimedOutboxRecord {
    id: Uuid,
    idempotency_key: String,
    op: String,
    tuple_user: String,
    tuple_relation: String,
    tuple_object: String,
    attempts: i32,
    lease_token: Uuid,
}

impl ClaimedOutboxRecord {
    fn row(&self) -> OutboxRow {
        OutboxRow {
            id: self.id,
            idempotency_key: self.idempotency_key.clone(),
            op: self.op.clone(),
            tuple_user: self.tuple_user.clone(),
            tuple_relation: self.tuple_relation.clone(),
            tuple_object: self.tuple_object.clone(),
            attempts: self.attempts,
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

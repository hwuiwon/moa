//! Background worker that drains `authz_outbox` rows into OpenFGA.

use crate::client::FgaClient;
use crate::error::AuthzError;
use crate::outbox::OutboxRow;
use serde_json::json;
use sqlx::PgPool;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::sleep;

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
}

impl Default for PollerConfig {
    fn default() -> Self {
        Self {
            batch_size: 64,
            poll_interval: Duration::from_millis(500),
            max_attempts: 8,
            backoff_base: Duration::from_millis(200),
            backoff_cap: Duration::from_secs(60),
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
        let mut tx = self.pool.begin().await?;
        let claimed: Vec<OutboxRow> = sqlx::query_as(
            r#"
            SELECT id, idempotency_key, op, tuple_user, tuple_relation, tuple_object, attempts
            FROM authz_outbox
            WHERE status = 'pending'
              AND next_attempt_at <= NOW()
            ORDER BY next_attempt_at
            LIMIT $1
            FOR UPDATE SKIP LOCKED
            "#,
        )
        .bind(limit_i64(self.cfg.batch_size))
        .fetch_all(&mut *tx)
        .await?;

        if claimed.is_empty() {
            tx.commit().await?;
            return Ok(0);
        }

        let ids: Vec<uuid::Uuid> = claimed.iter().map(|row| row.id).collect();
        sqlx::query(
            "UPDATE authz_outbox SET status='in_flight', updated_at=NOW() WHERE id = ANY($1)",
        )
        .bind(&ids)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        let mut applied = 0usize;
        for row in claimed {
            match self.apply_row(&row).await {
                Ok(()) => {
                    sqlx::query(
                        "UPDATE authz_outbox SET status='succeeded', updated_at=NOW() WHERE id=$1",
                    )
                    .bind(row.id)
                    .execute(&self.pool)
                    .await?;
                    applied += 1;
                }
                Err(error) => {
                    self.record_failure(&row, &error).await?;
                }
            }
        }

        Ok(applied)
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

    async fn record_failure(&self, row: &OutboxRow, error: &AuthzError) -> Result<(), AuthzError> {
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
                SET status='dead_letter', attempts=$2, last_error=$3, updated_at=NOW()
                WHERE id=$1
                "#,
            )
            .bind(row.id)
            .bind(next_attempts)
            .bind(error.to_string())
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
                updated_at=NOW()
            WHERE id=$1
            "#,
        )
        .bind(row.id)
        .bind(next_attempts)
        .bind(error.to_string())
        .bind(backoff.as_millis().to_string())
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

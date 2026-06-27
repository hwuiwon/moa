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

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::{PgPool, postgres::PgPoolOptions};

    #[tokio::test]
    async fn poller_reclaims_stale_in_flight_row() {
        // Pins: a worker crash after marking a row in_flight does not strand the tuple write.
        let pool = test_pool().await;
        let row_id = Uuid::new_v4();
        let stale_token = Uuid::new_v4();
        insert_outbox_row(
            &pool,
            row_id,
            "in_flight",
            Some(stale_token),
            "NOW() - INTERVAL '5 minutes'",
        )
        .await;
        let poller = poller(pool.clone(), 1);

        let claimed = poller
            .claim_batch()
            .await
            .expect("stale row should be claimable");

        assert_eq!(claimed.len(), 1, "exactly the stale row should be claimed");
        assert_eq!(claimed[0].row.id, row_id);
        assert_ne!(
            claimed[0].lease_token, stale_token,
            "reclaim must replace the stale lease token"
        );
        let (status, lease_token): (String, Option<Uuid>) =
            sqlx::query_as("SELECT status, lease_token FROM authz_outbox WHERE id = $1")
                .bind(row_id)
                .fetch_one(&pool)
                .await
                .expect("claimed row should be readable");
        assert_eq!(status, "in_flight");
        assert_eq!(lease_token, Some(claimed[0].lease_token));
    }

    #[tokio::test]
    async fn concurrent_pollers_claim_pending_row_once() {
        // Pins: multiple pods racing on the same pending row cannot both own the lease.
        let pool = test_pool().await;
        let row_id = Uuid::new_v4();
        insert_outbox_row(&pool, row_id, "pending", None, "NULL").await;
        let first = poller(pool.clone(), 1);
        let second = poller(pool.clone(), 1);

        let (first_claim, second_claim) = tokio::join!(first.claim_batch(), second.claim_batch());
        let first_claim = first_claim.expect("first claim query should succeed");
        let second_claim = second_claim.expect("second claim query should succeed");
        let total_claimed = first_claim.len() + second_claim.len();

        assert_eq!(
            total_claimed, 1,
            "competing pollers must produce exactly one claim"
        );
        let claimed_id = first_claim
            .first()
            .or_else(|| second_claim.first())
            .map(|claim| claim.row.id);
        assert_eq!(claimed_id, Some(row_id));
    }

    async fn test_pool() -> PgPool {
        let database_url = std::env::var("MOA_DATABASE_URL")
            .unwrap_or_else(|_| "postgres://moa_owner:dev@localhost:10040/moa".to_string());
        let schema_name = format!("authz_poller_test_{}", Uuid::new_v4().simple());
        let search_path = format!("{}, public", quote_identifier(&schema_name));
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(5))
            .after_connect(move |conn, _meta| {
                let search_path = search_path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT pg_catalog.set_config('search_path', $1, false)")
                        .bind(search_path)
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(&database_url)
            .await
            .expect("test Postgres should be reachable");
        moa_migrations::run_auth_schema(&pool, &schema_name)
            .await
            .expect("auth schema should apply");
        pool
    }

    fn poller(pool: PgPool, batch_size: usize) -> OutboxPoller {
        let client = FgaClient::new(crate::client::FgaConfig {
            url: "http://127.0.0.1:9".to_string(),
            preshared_key: "test".to_string(),
            store_id: "store".to_string(),
            model_id: "model".to_string(),
            timeout_ms: 100,
        })
        .expect("test FGA config should be valid");
        OutboxPoller::new(
            pool,
            client,
            PollerConfig {
                batch_size,
                poll_interval: Duration::from_millis(10),
                lease_duration: Duration::from_secs(60),
                ..PollerConfig::default()
            },
        )
    }

    async fn insert_outbox_row(
        pool: &PgPool,
        row_id: Uuid,
        status: &str,
        lease_token: Option<Uuid>,
        lease_expires_at_sql: &str,
    ) {
        let sql = format!(
            r#"
            INSERT INTO authz_outbox
                (id, idempotency_key, op, tuple_user, tuple_relation, tuple_object,
                 model_version, status, next_attempt_at, lease_token, lease_expires_at)
            VALUES
                ($1, $2, 'write', $3, 'operator', $4, 1, $5,
                 NOW() - INTERVAL '1 minute', $6, {lease_expires_at_sql})
            "#
        );
        sqlx::query(&sql)
            .bind(row_id)
            .bind(format!("test-key-{row_id}"))
            .bind(format!("user:{}", Uuid::new_v4()))
            .bind(format!("tenant:{}", Uuid::new_v4()))
            .bind(status)
            .bind(lease_token)
            .execute(pool)
            .await
            .expect("insert outbox row should succeed");
    }

    fn quote_identifier(identifier: &str) -> String {
        format!("\"{}\"", identifier.replace('"', "\"\""))
    }
}

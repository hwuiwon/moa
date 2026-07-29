//! Durable destruction owner for expired, stale, and abandoned hand leases.
//!
//! A hard maximum lifetime is only a policy if something destroys the sandbox
//! when it fires. Nothing in the request path can do that: a sandbox whose
//! session never sends another tool call is exactly the one that most needs
//! destroying, and no future traffic will arrive to trigger it. This reaper is
//! that owner. It runs independently of traffic, claims work in Postgres with
//! `FOR UPDATE ... SKIP LOCKED` so competing replicas never destroy the same
//! generation twice, and finalizes only the generation it claimed.
//!
//! A claimed generation moves to [`HandLeaseStatus::Reaping`], which
//! provisioning treats as owned. From there it is finalized as `Destroyed` on
//! success or released back to `Stale` behind a bounded backoff on failure. It
//! is never returned to `Active`: a sandbox the reaper decided to destroy is
//! not a sandbox anyone should get back.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures_util::{StreamExt, stream};
use moa_core::{
    error::MoaError, error::Result, traits::HandProvider, types::hands::HandHandle,
    types::identifiers::SessionId,
};
use sqlx::{PgPool, Row};
use tokio::task::JoinHandle;
use uuid::Uuid;

use super::leases::{LeaseHandle, map_sqlx_error};

/// One generation the reaper owns and must destroy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedHandLease {
    /// Session that owned the hand.
    pub session_id: SessionId,
    /// Worker scope within the session.
    pub worker_id: String,
    /// Provider that owns the sandbox.
    pub provider: String,
    /// Fenced generation. Finalization matches on it exactly.
    pub generation: i64,
    /// Unique ownership token for this destroy attempt.
    pub claim_token: Uuid,
    /// Durable handle to destroy.
    pub handle: LeaseHandle,
    /// Why this generation was claimed, for telemetry only.
    pub reason: HandLeaseDestroyReason,
    /// Consecutive failed destroy attempts before this one.
    pub attempts: i32,
}

/// Why the reaper claimed a generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandLeaseDestroyReason {
    /// The immutable hard maximum lifetime elapsed.
    HardLifetime,
    /// The renewable idle deadline elapsed.
    Idle,
    /// The lease was already stale or failed and still holds a live sandbox.
    Abandoned,
}

impl HandLeaseDestroyReason {
    /// Returns the stable telemetry label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HardLifetime => "hard_lifetime",
            Self::Idle => "idle",
            Self::Abandoned => "abandoned",
        }
    }

    /// Classifies one claimed row from its deadlines.
    fn classify(
        now: DateTime<Utc>,
        hard_expires_at: Option<DateTime<Utc>>,
        idle_expires_at: Option<DateTime<Utc>>,
    ) -> Self {
        if hard_expires_at.is_some_and(|hard| hard <= now) {
            Self::HardLifetime
        } else if idle_expires_at.is_some_and(|idle| idle <= now) {
            Self::Idle
        } else {
            Self::Abandoned
        }
    }
}

/// Durable claim surface the reaper drives.
#[async_trait::async_trait]
pub trait ExpiredHandLeaseClaims: Send + Sync {
    /// Claims up to `limit` destroyable generations, fencing each one.
    async fn claim_expired(&self, limit: i64, claim_ttl: Duration)
    -> Result<Vec<ClaimedHandLease>>;

    /// Marks a claimed generation destroyed.
    async fn finalize_destroyed(&self, claimed: &ClaimedHandLease) -> Result<bool>;

    /// Releases a claimed generation for a later retry behind a backoff.
    async fn release_for_retry(
        &self,
        claimed: &ClaimedHandLease,
        retry_after: Duration,
    ) -> Result<bool>;
}

/// Postgres-backed claim surface.
#[derive(Clone)]
pub struct PostgresExpiredHandLeaseClaims {
    pool: PgPool,
}

impl PostgresExpiredHandLeaseClaims {
    /// Creates the claim surface from an existing pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl ExpiredHandLeaseClaims for PostgresExpiredHandLeaseClaims {
    async fn claim_expired(
        &self,
        limit: i64,
        claim_ttl: Duration,
    ) -> Result<Vec<ClaimedHandLease>> {
        // `FOR UPDATE ... SKIP LOCKED` inside the CTE is what makes competing
        // replicas safe: each replica locks a disjoint set of rows and moves on
        // instead of blocking, and the outer UPDATE re-states the generation so
        // a row that changed generation between select and update is not
        // claimed. `LIMIT` keeps one pass bounded.
        let rows = sqlx::query(
            r#"
            WITH claimable AS (
                SELECT session_id, worker_id, provider, generation
                FROM moa.hand_leases
                WHERE handle IS NOT NULL
                  AND (
                        (
                            status IN ('active', 'provisioning', 'stale', 'failed')
                            AND (reap_not_before IS NULL OR reap_not_before <= now())
                            AND (
                                  (hard_expires_at IS NOT NULL AND hard_expires_at <= now())
                               OR (idle_expires_at IS NOT NULL AND idle_expires_at <= now())
                               OR status IN ('stale', 'failed')
                            )
                        )
                     OR (
                            status = 'reaping'
                            AND reap_claim_expires_at <= now()
                        )
                  )
                ORDER BY updated_at
                LIMIT $1
                FOR UPDATE SKIP LOCKED
            )
            UPDATE moa.hand_leases AS lease
            SET status = 'reaping',
                updated_at = now(),
                reap_claim_token = gen_random_uuid(),
                reap_claim_expires_at = now() + make_interval(secs => $2)
            FROM claimable
            WHERE lease.session_id = claimable.session_id
              AND lease.worker_id = claimable.worker_id
              AND lease.provider = claimable.provider
              AND lease.generation = claimable.generation
            RETURNING lease.session_id, lease.worker_id, lease.provider, lease.generation,
                      lease.handle, lease.hard_expires_at, lease.idle_expires_at,
                      lease.reap_attempts, lease.reap_claim_token
            "#,
        )
        .bind(limit)
        .bind(claim_ttl.as_secs_f64())
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        let now = Utc::now();
        rows.iter()
            .map(|row| {
                let handle = row
                    .try_get::<Option<sqlx::types::Json<LeaseHandle>>, _>("handle")
                    .map_err(map_sqlx_error)?
                    .ok_or_else(|| {
                        MoaError::StorageError(
                            "claimed hand lease lost its handle between select and update"
                                .to_string(),
                        )
                    })?;
                Ok(ClaimedHandLease {
                    session_id: row.try_get("session_id").map_err(map_sqlx_error)?,
                    worker_id: row.try_get("worker_id").map_err(map_sqlx_error)?,
                    provider: row.try_get("provider").map_err(map_sqlx_error)?,
                    generation: row.try_get("generation").map_err(map_sqlx_error)?,
                    claim_token: row.try_get("reap_claim_token").map_err(map_sqlx_error)?,
                    handle: handle.0,
                    reason: HandLeaseDestroyReason::classify(
                        now,
                        row.try_get("hard_expires_at").map_err(map_sqlx_error)?,
                        row.try_get("idle_expires_at").map_err(map_sqlx_error)?,
                    ),
                    attempts: row.try_get("reap_attempts").map_err(map_sqlx_error)?,
                })
            })
            .collect()
    }

    async fn finalize_destroyed(&self, claimed: &ClaimedHandLease) -> Result<bool> {
        let affected = sqlx::query(
            r#"
            UPDATE moa.hand_leases
            SET status = 'destroyed',
                handle = NULL,
                updated_at = now(),
                reap_attempts = 0,
                reap_not_before = NULL,
                reap_claim_token = NULL,
                reap_claim_expires_at = NULL
            WHERE session_id = $1
              AND worker_id = $2
              AND provider = $3
              AND generation = $4
              AND status = 'reaping'
              AND reap_claim_token = $5
            "#,
        )
        .bind(claimed.session_id)
        .bind(&claimed.worker_id)
        .bind(&claimed.provider)
        .bind(claimed.generation)
        .bind(claimed.claim_token)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        Ok(affected == 1)
    }

    async fn release_for_retry(
        &self,
        claimed: &ClaimedHandLease,
        retry_after: Duration,
    ) -> Result<bool> {
        // Released to `stale`, never to `active`: the sandbox stays fenced off
        // from reuse while it waits for another destroy attempt.
        let affected = sqlx::query(
            r#"
            UPDATE moa.hand_leases
            SET status = 'stale',
                updated_at = now(),
                reap_attempts = reap_attempts + 1,
                reap_not_before = now() + make_interval(secs => $5),
                reap_claim_token = NULL,
                reap_claim_expires_at = NULL
            WHERE session_id = $1
              AND worker_id = $2
              AND provider = $3
              AND generation = $4
              AND status = 'reaping'
              AND reap_claim_token = $6
            "#,
        )
        .bind(claimed.session_id)
        .bind(&claimed.worker_id)
        .bind(&claimed.provider)
        .bind(claimed.generation)
        .bind(retry_after.as_secs_f64())
        .bind(claimed.claim_token)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        Ok(affected == 1)
    }
}

/// Reaper pacing and batch sizing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandLeaseReaperConfig {
    /// Delay between sweeps.
    pub interval: Duration,
    /// Maximum generations claimed in one sweep.
    pub batch_size: i64,
    /// How long one destroy claim remains owned before another replica may recover it.
    pub claim_ttl: Duration,
    /// Maximum provider destroys driven concurrently by one replica.
    pub max_destroy_concurrency: usize,
    /// Backoff applied after the first failed destroy attempt.
    pub base_retry_delay: Duration,
    /// Ceiling the exponential backoff never exceeds.
    pub max_retry_delay: Duration,
}

impl Default for HandLeaseReaperConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(30),
            batch_size: 64,
            claim_ttl: Duration::from_secs(5 * 60),
            max_destroy_concurrency: 4,
            base_retry_delay: Duration::from_secs(15),
            max_retry_delay: Duration::from_secs(15 * 60),
        }
    }
}

impl HandLeaseReaperConfig {
    /// Returns the backoff for the next attempt after `attempts` failures.
    #[must_use]
    pub fn retry_delay(&self, attempts: i32) -> Duration {
        let shift = u32::try_from(attempts.max(0)).unwrap_or(u32::MAX).min(16);
        self.base_retry_delay
            .saturating_mul(1_u32 << shift)
            .min(self.max_retry_delay)
    }
}

/// Independent durable reaper for expired and abandoned sandboxes.
pub struct HandLeaseReaper {
    claims: Arc<dyn ExpiredHandLeaseClaims>,
    providers: Vec<Arc<dyn HandProvider>>,
    config: HandLeaseReaperConfig,
}

impl HandLeaseReaper {
    /// Creates a reaper over one claim surface and the providers that can
    /// destroy the sandboxes it will claim.
    #[must_use]
    pub fn new(
        claims: Arc<dyn ExpiredHandLeaseClaims>,
        providers: Vec<Arc<dyn HandProvider>>,
        config: HandLeaseReaperConfig,
    ) -> Self {
        Self {
            claims,
            providers,
            config,
        }
    }

    /// Runs one bounded sweep and returns how many generations were destroyed.
    pub async fn sweep(&self) -> Result<usize> {
        let claimed = self
            .claims
            .claim_expired(self.config.batch_size, self.config.claim_ttl)
            .await?;
        let outcomes = stream::iter(claimed)
            .map(|lease| async move {
                match self.destroy_claimed(&lease).await {
                    Ok(()) => {
                        if !self.claims.finalize_destroyed(&lease).await? {
                            tracing::warn!(
                                provider = %lease.provider,
                                generation = lease.generation,
                                destroy_reason = lease.reason.as_str(),
                                "durable reaper destroy completed after its claim fence was lost"
                            );
                            return Ok(0_usize);
                        }
                        tracing::info!(
                            provider = %lease.provider,
                            generation = lease.generation,
                            destroy_reason = lease.reason.as_str(),
                            "durable reaper destroyed an expired sandbox"
                        );
                        Ok(1_usize)
                    }
                    Err(error) => {
                        let retry_after = self.config.retry_delay(lease.attempts);
                        if !self.claims.release_for_retry(&lease, retry_after).await? {
                            tracing::warn!(
                                provider = %lease.provider,
                                generation = lease.generation,
                                destroy_reason = lease.reason.as_str(),
                                "durable reaper destroy failed after its claim fence was lost"
                            );
                            return Ok(0);
                        }
                        tracing::warn!(
                            provider = %lease.provider,
                            generation = lease.generation,
                            destroy_reason = lease.reason.as_str(),
                            attempts = lease.attempts + 1,
                            retry_after_secs = retry_after.as_secs(),
                            error = %error,
                            "durable reaper could not destroy a sandbox; staying fenced for retry"
                        );
                        Ok(0)
                    }
                }
            })
            .buffer_unordered(self.config.max_destroy_concurrency.max(1))
            .collect::<Vec<Result<usize>>>()
            .await;
        let mut destroyed = 0;
        for outcome in outcomes {
            destroyed += outcome?;
        }
        Ok(destroyed)
    }

    /// Destroys one claimed generation through its owning provider.
    async fn destroy_claimed(&self, lease: &ClaimedHandLease) -> Result<()> {
        let provider = self
            .providers
            .iter()
            .find(|provider| provider.provider_name() == lease.provider)
            .ok_or_else(|| {
                MoaError::ProviderError(format!(
                    "no registered hand provider named {} can destroy this sandbox",
                    lease.provider
                ))
            })?;
        provider.destroy(&hand_handle(lease)).await
    }

    /// Spawns the sweep loop and returns its join handle.
    #[must_use]
    pub fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(self.config.interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if let Err(error) = self.sweep().await {
                    tracing::warn!(
                        error = %error,
                        "durable hand-lease reaper sweep failed; retrying next interval"
                    );
                }
            }
        })
    }
}

/// Returns the provider handle a claimed lease destroys.
fn hand_handle(lease: &ClaimedHandLease) -> HandHandle {
    lease.handle.handle.clone()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct RecordingClaims {
        claimed: Mutex<Vec<ClaimedHandLease>>,
        finalized: Mutex<Vec<i64>>,
        released: Mutex<Vec<(i64, Duration)>>,
    }

    #[async_trait::async_trait]
    impl ExpiredHandLeaseClaims for RecordingClaims {
        async fn claim_expired(
            &self,
            _limit: i64,
            _claim_ttl: Duration,
        ) -> Result<Vec<ClaimedHandLease>> {
            Ok(std::mem::take(
                &mut *self.claimed.lock().expect("claimed lock"),
            ))
        }

        async fn finalize_destroyed(&self, claimed: &ClaimedHandLease) -> Result<bool> {
            self.finalized
                .lock()
                .expect("finalized lock")
                .push(claimed.generation);
            Ok(true)
        }

        async fn release_for_retry(
            &self,
            claimed: &ClaimedHandLease,
            retry_after: Duration,
        ) -> Result<bool> {
            self.released
                .lock()
                .expect("released lock")
                .push((claimed.generation, retry_after));
            Ok(true)
        }
    }

    struct StubProvider {
        destroy_fails: bool,
    }

    #[async_trait::async_trait]
    impl HandProvider for StubProvider {
        fn provider_name(&self) -> &str {
            "local"
        }

        fn capabilities(&self) -> moa_core::types::hands::HandProviderCapabilities {
            crate::adapters::local::LOCAL_HAND_CAPABILITIES.clone()
        }

        async fn provision(
            &self,
            _spec: moa_core::types::hands::HandSpec,
        ) -> Result<moa_core::types::hands::HandHandle> {
            Err(MoaError::Unsupported("stub".to_string()))
        }

        async fn execute(
            &self,
            _handle: &HandHandle,
            _tool: &str,
            _input: &str,
        ) -> Result<moa_core::types::tools::ToolOutput> {
            Err(MoaError::Unsupported("stub".to_string()))
        }

        async fn status(&self, _handle: &HandHandle) -> Result<moa_core::types::hands::HandStatus> {
            Ok(moa_core::types::hands::HandStatus::Running)
        }

        async fn pause(&self, _handle: &HandHandle) -> Result<()> {
            Ok(())
        }

        async fn resume(&self, _handle: &HandHandle) -> Result<()> {
            Ok(())
        }

        async fn destroy(&self, _handle: &HandHandle) -> Result<()> {
            if self.destroy_fails {
                return Err(MoaError::ProviderError("destroy failed".to_string()));
            }
            Ok(())
        }
    }

    fn claimed(generation: i64, attempts: i32) -> ClaimedHandLease {
        ClaimedHandLease {
            session_id: SessionId::new(),
            worker_id: String::new(),
            provider: "local".to_string(),
            generation,
            claim_token: Uuid::new_v4(),
            handle: LeaseHandle::new(HandHandle::local(std::path::PathBuf::from("/tmp/moa-reap"))),
            reason: HandLeaseDestroyReason::HardLifetime,
            attempts,
        }
    }

    #[tokio::test]
    async fn sweep_destroys_claimed_generations_without_any_new_traffic() {
        // Pins: destruction is driven by the reaper's own sweep, not by a later
        // tool call — a session that never sends another request still has its
        // hard-expired sandbox destroyed and its exact generation finalized.
        let claims = Arc::new(RecordingClaims::default());
        *claims.claimed.lock().expect("claimed lock") = vec![claimed(7, 0)];
        let reaper = HandLeaseReaper::new(
            claims.clone(),
            vec![Arc::new(StubProvider {
                destroy_fails: false,
            })],
            HandLeaseReaperConfig::default(),
        );

        assert_eq!(reaper.sweep().await.expect("sweep"), 1);
        assert_eq!(*claims.finalized.lock().expect("finalized lock"), vec![7]);
        assert!(claims.released.lock().expect("released lock").is_empty());
    }

    #[tokio::test]
    async fn destroy_failure_stays_fenced_and_retryable_with_backoff() {
        // Pins: a failed destroy releases the generation for retry behind a
        // growing backoff and never finalizes it, so the sandbox is never
        // handed back as reusable.
        let claims = Arc::new(RecordingClaims::default());
        *claims.claimed.lock().expect("claimed lock") = vec![claimed(3, 2)];
        let config = HandLeaseReaperConfig::default();
        let reaper = HandLeaseReaper::new(
            claims.clone(),
            vec![Arc::new(StubProvider {
                destroy_fails: true,
            })],
            config,
        );

        assert_eq!(reaper.sweep().await.expect("sweep"), 0);
        assert!(claims.finalized.lock().expect("finalized lock").is_empty());
        assert_eq!(
            *claims.released.lock().expect("released lock"),
            vec![(3, config.retry_delay(2))]
        );
    }

    #[test]
    fn retry_backoff_grows_and_is_capped() {
        // Pins: repeated destroy failures back off exponentially and stop at the
        // configured ceiling instead of overflowing or retrying forever at speed.
        let config = HandLeaseReaperConfig::default();
        assert_eq!(config.retry_delay(0), Duration::from_secs(15));
        assert_eq!(config.retry_delay(1), Duration::from_secs(30));
        assert_eq!(config.retry_delay(2), Duration::from_secs(60));
        assert_eq!(config.retry_delay(64), config.max_retry_delay);
    }

    #[test]
    fn destroy_reason_reports_the_deadline_that_fired() {
        // Pins: telemetry names which deadline caused destruction, with the hard
        // lifetime taking precedence over idle when both have elapsed.
        let now = Utc::now();
        let past = now - chrono::Duration::seconds(1);
        let future = now + chrono::Duration::hours(1);
        assert_eq!(
            HandLeaseDestroyReason::classify(now, Some(past), Some(past)),
            HandLeaseDestroyReason::HardLifetime
        );
        assert_eq!(
            HandLeaseDestroyReason::classify(now, Some(future), Some(past)),
            HandLeaseDestroyReason::Idle
        );
        assert_eq!(
            HandLeaseDestroyReason::classify(now, None, None),
            HandLeaseDestroyReason::Abandoned
        );
    }
}

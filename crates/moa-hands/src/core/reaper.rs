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
//! success or released back to `Failed` behind a bounded backoff on failure. It
//! is never returned to `Active`: a sandbox the reaper decided to destroy is
//! not a sandbox anyone should get back.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use futures_util::{StreamExt, stream};
#[cfg(test)]
use moa_core::types::hands::HandHandle;
use moa_core::{
    error::MoaError, error::Result, traits::HandProvider,
    types::identifiers::HandProvisioningOperationId, types::identifiers::ProviderAccountId,
    types::identifiers::SandboxWorkspaceId, types::identifiers::SessionId,
    types::identifiers::TenantId, types::identifiers::WorkspaceCheckpointId,
};
use moa_db::ScopedConn;
use sqlx::{PgConnection, PgPool, Row};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::leases::{
    HandLeaseWorkspaceAttachment, LeaseHandle, PROVISIONING_EMPTY_CONFIRMATION,
    PROVISIONING_VISIBILITY_GRACE, map_sqlx_error,
};
use super::sandbox_workspace::capacity::release_active_hand_for_reaper_in_transaction;

/// One generation the reaper owns and must destroy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedHandLease {
    /// Tenant that owns the lease; maintenance updates repeat it in every fence.
    pub tenant_id: TenantId,
    /// Session that owned the hand.
    pub session_id: SessionId,
    /// Worker scope within the session.
    pub worker_id: String,
    /// Provider that owns the sandbox.
    pub provider: String,
    /// Persisted provider account used for discovery after restart.
    pub provider_account_id: ProviderAccountId,
    /// Persisted provider-account generation used for discovery.
    pub provider_account_generation: u64,
    /// Fenced generation. Finalization matches on it exactly.
    pub generation: i64,
    /// Durable provider-visible create identity for this generation.
    pub provisioning_operation_id: HandProvisioningOperationId,
    /// Unique ownership token for this destroy attempt.
    pub claim_token: Uuid,
    /// Durable handle to destroy, when activation recorded one.
    pub handle: Option<LeaseHandle>,
    /// Exact workspace attachment present when this generation was claimed.
    ///
    /// Terminal legacy rows may have no attachment. Any attached row carries
    /// all four fields together so later renew, retry, and finalize updates
    /// cannot alter or release a newer workspace attachment.
    pub attachment: Option<HandLeaseWorkspaceAttachment>,
    /// Earliest durable time at which an ambiguous create may be reconciled.
    pub reconcile_not_before: Option<DateTime<Utc>>,
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

    /// Renews one exact reaper claim while provider reconciliation is in flight.
    async fn renew_claim(&self, claimed: &ClaimedHandLease, claim_ttl: Duration) -> Result<bool>;
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

    /// Begins the closed fleet-maintenance transaction used only by the reaper.
    async fn begin_maintenance(&self) -> Result<ScopedConn<'_>> {
        let mut conn = ScopedConn::begin_control_plane(&self.pool).await?;
        conn.assume_app_role().await?;
        Ok(conn)
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
        let mut conn = self.begin_maintenance().await?;
        let rows = sqlx::query(
            r#"
            WITH claimable AS (
                SELECT tenant_id, session_id, worker_id, provider, generation,
                       provisioning_operation_id, status, handle,
                       workspace_id, workspace_writer_epoch,
                       workspace_instance_generation, restored_checkpoint_id
                FROM moa.hand_leases
                WHERE (
                        status = 'provisioning'
                    AND reap_not_before <= now()
                ) OR (
                        status = 'failed'
                    AND reap_not_before <= now()
                ) OR (
                        status IN ('active', 'stale')
                    AND handle IS NOT NULL
                    AND (
                           status = 'stale'
                        OR (hard_expires_at IS NOT NULL AND hard_expires_at <= now())
                        OR (idle_expires_at IS NOT NULL AND idle_expires_at <= now())
                    )
                ) OR (
                        status = 'reaping'
                    AND reap_claim_expires_at <= now()
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
              AND lease.tenant_id = claimable.tenant_id
              AND lease.worker_id = claimable.worker_id
              AND lease.provider = claimable.provider
              AND lease.generation = claimable.generation
              AND lease.provisioning_operation_id = claimable.provisioning_operation_id
              AND lease.status = claimable.status
              AND lease.handle IS NOT DISTINCT FROM claimable.handle
              AND lease.workspace_id IS NOT DISTINCT FROM claimable.workspace_id
              AND lease.workspace_writer_epoch IS NOT DISTINCT FROM claimable.workspace_writer_epoch
              AND lease.workspace_instance_generation IS NOT DISTINCT FROM claimable.workspace_instance_generation
              AND lease.restored_checkpoint_id IS NOT DISTINCT FROM claimable.restored_checkpoint_id
            RETURNING lease.tenant_id, lease.session_id, lease.worker_id, lease.provider, lease.generation,
                      lease.provisioning_operation_id, lease.handle, lease.status,
                      lease.workspace_id, lease.workspace_writer_epoch,
                      lease.workspace_instance_generation, lease.restored_checkpoint_id,
                      (SELECT workspace.provider_account_id
                         FROM moa.sandbox_workspaces AS workspace
                        WHERE workspace.workspace_id = lease.workspace_id
                          AND workspace.tenant_id = lease.tenant_id) AS provider_account_id,
                      (SELECT workspace.provider_account_generation
                         FROM moa.sandbox_workspaces AS workspace
                        WHERE workspace.workspace_id = lease.workspace_id
                          AND workspace.tenant_id = lease.tenant_id) AS provider_account_generation,
                      lease.hard_expires_at, lease.idle_expires_at,
                      lease.reap_not_before, lease.reap_attempts, lease.reap_claim_token
            "#,
        )
        .bind(limit)
        .bind(claim_ttl.as_secs_f64())
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;

        let now = Utc::now();
        let claimed = rows
            .iter()
            .map(|row| {
                let handle = row
                    .try_get::<Option<sqlx::types::Json<LeaseHandle>>, _>("handle")
                    .map_err(map_sqlx_error)?;
                let attachment = claimed_attachment_from_row(row)?;
                let handle_account = handle
                    .as_ref()
                    .and_then(|handle| handle.0.handle.provider_account());
                let provider_account_id = row
                    .try_get::<Option<ProviderAccountId>, _>("provider_account_id")
                    .map_err(map_sqlx_error)?
                    .or_else(|| handle_account.map(|context| context.0))
                    .unwrap_or(ProviderAccountId(Uuid::nil()));
                let provider_account_generation = row
                    .try_get::<Option<i64>, _>("provider_account_generation")
                    .map_err(map_sqlx_error)?
                    .and_then(|generation| u64::try_from(generation).ok())
                    .or_else(|| handle_account.map(|context| context.1))
                    .unwrap_or(0);
                Ok(ClaimedHandLease {
                    tenant_id: row.try_get("tenant_id").map_err(map_sqlx_error)?,
                    session_id: row.try_get("session_id").map_err(map_sqlx_error)?,
                    worker_id: row.try_get("worker_id").map_err(map_sqlx_error)?,
                    provider: row.try_get("provider").map_err(map_sqlx_error)?,
                    provider_account_id,
                    provider_account_generation,
                    generation: row.try_get("generation").map_err(map_sqlx_error)?,
                    provisioning_operation_id: row
                        .try_get("provisioning_operation_id")
                        .map_err(map_sqlx_error)?,
                    claim_token: row.try_get("reap_claim_token").map_err(map_sqlx_error)?,
                    handle: handle.map(|handle| handle.0),
                    attachment,
                    reconcile_not_before: row.try_get("reap_not_before").map_err(map_sqlx_error)?,
                    reason: HandLeaseDestroyReason::classify(
                        now,
                        row.try_get("hard_expires_at").map_err(map_sqlx_error)?,
                        row.try_get("idle_expires_at").map_err(map_sqlx_error)?,
                    ),
                    attempts: row.try_get("reap_attempts").map_err(map_sqlx_error)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        conn.commit().await?;
        Ok(claimed)
    }

    async fn finalize_destroyed(&self, claimed: &ClaimedHandLease) -> Result<bool> {
        let mut conn = self.begin_maintenance().await?;
        if !transition_attached_workspace_after_destroy(conn.as_mut(), claimed).await? {
            conn.rollback().await?;
            return Ok(false);
        }
        if claimed.attachment.is_some()
            && !release_active_hand_for_reaper_in_transaction(
                conn.as_mut(),
                claimed.tenant_id,
                claimed.provisioning_operation_id,
                claimed.generation,
                claimed.claim_token,
            )
            .await?
        {
            conn.rollback().await?;
            return Ok(false);
        }
        let affected = sqlx::query(
            r#"
            UPDATE moa.hand_leases
            SET status = 'destroyed',
                handle = NULL,
                workspace_id = NULL,
                workspace_writer_epoch = NULL,
                workspace_instance_generation = NULL,
                restored_checkpoint_id = NULL,
                updated_at = now(),
                reap_attempts = 0,
                reap_not_before = NULL,
                reap_claim_token = NULL,
                reap_claim_expires_at = NULL
            WHERE session_id = $1
              AND worker_id = $2
              AND provider = $3
              AND generation = $4
              AND provisioning_operation_id = $5
              AND status = 'reaping'
              AND handle IS NOT DISTINCT FROM $6
              AND reap_claim_token = $7
              AND tenant_id = $8
              AND workspace_id IS NOT DISTINCT FROM $9
              AND workspace_writer_epoch IS NOT DISTINCT FROM $10
              AND workspace_instance_generation IS NOT DISTINCT FROM $11
              AND restored_checkpoint_id IS NOT DISTINCT FROM $12
            "#,
        )
        .bind(claimed.session_id)
        .bind(&claimed.worker_id)
        .bind(&claimed.provider)
        .bind(claimed.generation)
        .bind(claimed.provisioning_operation_id)
        .bind(claimed.handle.clone().map(sqlx::types::Json))
        .bind(claimed.claim_token)
        .bind(claimed.tenant_id)
        .bind(
            claimed
                .attachment
                .as_ref()
                .map(|attachment| attachment.workspace_id),
        )
        .bind(
            claimed
                .attachment
                .as_ref()
                .map(|attachment| attachment.workspace_writer_epoch),
        )
        .bind(
            claimed
                .attachment
                .as_ref()
                .map(|attachment| attachment.workspace_instance_generation),
        )
        .bind(
            claimed
                .attachment
                .as_ref()
                .and_then(|attachment| attachment.restored_checkpoint_id),
        )
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        conn.commit().await?;
        Ok(affected == 1)
    }

    async fn release_for_retry(
        &self,
        claimed: &ClaimedHandLease,
        retry_after: Duration,
    ) -> Result<bool> {
        // Released to `failed`, never to `stale` or `active`: request traffic
        // cannot overwrite the unresolved operation while cleanup backs off.
        //
        // The backoff never schedules the next attempt inside the provider
        // visibility grace. A lease whose idle window is shorter than its
        // provisioning deadline is reaped while that deadline is still in the
        // future, and a bare `now() + retry_after` would both violate the
        // cleanup-schedule invariant and retry before the provider could have
        // made a late create observable.
        let mut conn = self.begin_maintenance().await?;
        let affected = sqlx::query(
            r#"
            UPDATE moa.hand_leases
            SET status = 'failed',
                updated_at = now(),
                reap_attempts = reap_attempts + 1,
                reap_not_before = GREATEST(
                    now() + make_interval(secs => $7),
                    provisioning_deadline_at + make_interval(secs => $9)
                ),
                reap_claim_token = NULL,
                reap_claim_expires_at = NULL
            WHERE session_id = $1
              AND worker_id = $2
              AND provider = $3
              AND generation = $4
              AND provisioning_operation_id = $5
              AND status = 'reaping'
              AND handle IS NOT DISTINCT FROM $6
              AND reap_claim_token = $8
              AND tenant_id = $10
              AND workspace_id IS NOT DISTINCT FROM $11
              AND workspace_writer_epoch IS NOT DISTINCT FROM $12
              AND workspace_instance_generation IS NOT DISTINCT FROM $13
              AND restored_checkpoint_id IS NOT DISTINCT FROM $14
            "#,
        )
        .bind(claimed.session_id)
        .bind(&claimed.worker_id)
        .bind(&claimed.provider)
        .bind(claimed.generation)
        .bind(claimed.provisioning_operation_id)
        .bind(claimed.handle.clone().map(sqlx::types::Json))
        .bind(retry_after.as_secs_f64())
        .bind(claimed.claim_token)
        .bind(PROVISIONING_VISIBILITY_GRACE.as_secs_f64())
        .bind(claimed.tenant_id)
        .bind(
            claimed
                .attachment
                .as_ref()
                .map(|attachment| attachment.workspace_id),
        )
        .bind(
            claimed
                .attachment
                .as_ref()
                .map(|attachment| attachment.workspace_writer_epoch),
        )
        .bind(
            claimed
                .attachment
                .as_ref()
                .map(|attachment| attachment.workspace_instance_generation),
        )
        .bind(
            claimed
                .attachment
                .as_ref()
                .and_then(|attachment| attachment.restored_checkpoint_id),
        )
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        conn.commit().await?;
        Ok(affected == 1)
    }

    async fn renew_claim(&self, claimed: &ClaimedHandLease, claim_ttl: Duration) -> Result<bool> {
        let mut conn = self.begin_maintenance().await?;
        let affected = sqlx::query(
            r#"
            UPDATE moa.hand_leases
            SET updated_at = now(),
                reap_claim_expires_at = now() + make_interval(secs => $8)
            WHERE session_id = $1
              AND worker_id = $2
              AND provider = $3
              AND generation = $4
              AND provisioning_operation_id = $5
              AND status = 'reaping'
              AND handle IS NOT DISTINCT FROM $6
              AND reap_claim_token = $7
              AND reap_claim_expires_at > now()
              AND tenant_id = $9
              AND workspace_id IS NOT DISTINCT FROM $10
              AND workspace_writer_epoch IS NOT DISTINCT FROM $11
              AND workspace_instance_generation IS NOT DISTINCT FROM $12
              AND restored_checkpoint_id IS NOT DISTINCT FROM $13
            "#,
        )
        .bind(claimed.session_id)
        .bind(&claimed.worker_id)
        .bind(&claimed.provider)
        .bind(claimed.generation)
        .bind(claimed.provisioning_operation_id)
        .bind(claimed.handle.clone().map(sqlx::types::Json))
        .bind(claimed.claim_token)
        .bind(claim_ttl.as_secs_f64())
        .bind(claimed.tenant_id)
        .bind(
            claimed
                .attachment
                .as_ref()
                .map(|attachment| attachment.workspace_id),
        )
        .bind(
            claimed
                .attachment
                .as_ref()
                .map(|attachment| attachment.workspace_writer_epoch),
        )
        .bind(
            claimed
                .attachment
                .as_ref()
                .map(|attachment| attachment.workspace_instance_generation),
        )
        .bind(
            claimed
                .attachment
                .as_ref()
                .and_then(|attachment| attachment.restored_checkpoint_id),
        )
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        conn.commit().await?;
        Ok(affected == 1)
    }
}

/// Atomically detaches the exact workspace generation whose compute was destroyed.
async fn transition_attached_workspace_after_destroy(
    conn: &mut PgConnection,
    claimed: &ClaimedHandLease,
) -> Result<bool> {
    let Some(attachment) = &claimed.attachment else {
        // Legacy terminal leases may predate durable workspaces. There is no
        // workspace ownership row for this reaper generation to transition.
        return Ok(true);
    };
    let affected = sqlx::query(
        r#"
        UPDATE moa.sandbox_workspaces AS workspace
        SET lifecycle_state = CASE
                WHEN workspace.lifecycle_state IN ('creating', 'quiescing', 'committing', 'reconciling')
                  OR EXISTS (
                        SELECT 1
                        FROM moa.sandbox_workspace_operations AS operation
                        WHERE operation.tenant_id = workspace.tenant_id
                          AND operation.workspace_id = workspace.workspace_id
                          AND operation.provider_account_id = workspace.provider_account_id
                          AND operation.provider_account_generation = workspace.provider_account_generation
                          AND operation.expected_writer_epoch = workspace.writer_epoch
                          AND operation.expected_instance_generation = workspace.instance_generation
                          AND operation.outcome_class IN ('not_sent', 'unknown')
                          AND operation.operation_kind <> 'delete'
                    )
                THEN 'reconciling'
                WHEN workspace.lifecycle_state = 'failed' THEN 'failed'
                ELSE 'ready'
            END,
            updated_at = now()
        WHERE workspace.tenant_id = $1 AND workspace.workspace_id = $2
          AND workspace.provider_account_id = $3
          AND workspace.provider_account_generation = $4
          AND workspace.writer_epoch = $5
          AND workspace.instance_generation = $6
          AND workspace.current_checkpoint_id IS NOT DISTINCT FROM $7
          AND workspace.lifecycle_state NOT IN ('deleting', 'deleted')
        "#,
    )
    .bind(claimed.tenant_id)
    .bind(attachment.workspace_id)
    .bind(claimed.provider_account_id)
    .bind(i64::try_from(claimed.provider_account_generation).map_err(|_| {
        MoaError::ValidationError(
            "reaper provider-account generation exceeds Postgres bigint".to_string(),
        )
    })?)
    .bind(attachment.workspace_writer_epoch)
    .bind(attachment.workspace_instance_generation)
    .bind(attachment.restored_checkpoint_id)
    .execute(conn)
    .await
    .map_err(map_sqlx_error)?
    .rows_affected();
    Ok(affected == 1)
}

/// Decodes the all-or-none workspace attachment captured by a reaper claim.
fn claimed_attachment_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<Option<HandLeaseWorkspaceAttachment>> {
    let workspace_id = row
        .try_get::<Option<SandboxWorkspaceId>, _>("workspace_id")
        .map_err(map_sqlx_error)?;
    let writer_epoch = row
        .try_get::<Option<i64>, _>("workspace_writer_epoch")
        .map_err(map_sqlx_error)?;
    let instance_generation = row
        .try_get::<Option<i64>, _>("workspace_instance_generation")
        .map_err(map_sqlx_error)?;
    let restored_checkpoint_id = row
        .try_get::<Option<WorkspaceCheckpointId>, _>("restored_checkpoint_id")
        .map_err(map_sqlx_error)?;

    claimed_attachment(
        workspace_id,
        writer_epoch,
        instance_generation,
        restored_checkpoint_id,
    )
}

fn claimed_attachment(
    workspace_id: Option<SandboxWorkspaceId>,
    writer_epoch: Option<i64>,
    instance_generation: Option<i64>,
    restored_checkpoint_id: Option<WorkspaceCheckpointId>,
) -> Result<Option<HandLeaseWorkspaceAttachment>> {
    match (workspace_id, writer_epoch, instance_generation) {
        (Some(workspace_id), Some(writer_epoch), Some(instance_generation)) => {
            HandLeaseWorkspaceAttachment::new(
                workspace_id,
                writer_epoch,
                instance_generation,
                restored_checkpoint_id,
            )
            .map(Some)
        }
        (None, None, None) if restored_checkpoint_id.is_none() => Ok(None),
        _ => Err(MoaError::StorageError(
            "reaper claim contains a partial durable workspace attachment".to_string(),
        )),
    }
}

/// Reaper pacing and batch sizing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandLeaseReaperConfig {
    /// Delay between sweeps.
    pub interval: Duration,
    /// Maximum generations considered in one sweep.
    ///
    /// The reaper additionally caps each database claim to
    /// `max_destroy_concurrency` so no claimed row waits in an unpolled stream
    /// without its ownership heartbeat running.
    pub batch_size: i64,
    /// How long one destroy claim remains owned before another replica may recover it.
    pub claim_ttl: Duration,
    /// Maximum provider destroys driven concurrently by one replica.
    pub max_destroy_concurrency: usize,
    /// Backoff applied after the first failed destroy attempt.
    pub base_retry_delay: Duration,
    /// Ceiling the exponential backoff never exceeds.
    pub max_retry_delay: Duration,
    /// Maximum acceptable age of the last complete successful sweep.
    pub heartbeat_maximum_age: Duration,
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
            heartbeat_maximum_age: Duration::from_secs(90),
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

/// Supervised process handle for durable hand-lease cleanup.
pub struct HandLeaseReaperHandle {
    state: Arc<HandLeaseReaperHealth>,
    shutdown: CancellationToken,
    task: JoinHandle<Result<()>>,
    heartbeat_maximum_age: Duration,
}

/// Cloneable readiness projection for the supervised hand-lease reaper.
#[derive(Clone)]
pub struct HandLeaseReaperReadiness {
    state: Arc<HandLeaseReaperHealth>,
    heartbeat_maximum_age: Duration,
}

#[derive(Debug)]
struct HandLeaseReaperHealth {
    started_at: Instant,
    last_heartbeat: RwLock<Option<Instant>>,
    unready_reason: RwLock<Option<String>>,
    exited: std::sync::atomic::AtomicBool,
}

impl HandLeaseReaperHandle {
    /// Returns the age of the most recent complete successful sweep.
    #[must_use]
    pub fn heartbeat_age(&self) -> Duration {
        self.readiness().heartbeat_age()
    }

    /// Returns a cloneable health projection for process readiness.
    #[must_use]
    pub fn readiness(&self) -> HandLeaseReaperReadiness {
        HandLeaseReaperReadiness {
            state: Arc::clone(&self.state),
            heartbeat_maximum_age: self.heartbeat_maximum_age,
        }
    }

    /// Awaits the task result so unexpected exit can be process-fatal.
    pub async fn task_result(&mut self) -> Result<()> {
        match (&mut self.task).await {
            Ok(result) => result,
            Err(error) => Err(MoaError::StorageError(format!(
                "hand lease reaper task join failed: {error}"
            ))),
        }
    }

    /// Cancels and joins the supervised task during graceful shutdown.
    pub async fn shutdown(mut self) -> Result<()> {
        self.shutdown.cancel();
        match tokio::time::timeout(Duration::from_secs(10), &mut self.task).await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => Err(MoaError::StorageError(format!(
                "hand lease reaper task join failed: {error}"
            ))),
            Err(_) => {
                self.task.abort();
                let _ = (&mut self.task).await;
                Err(MoaError::StorageError(
                    "hand lease reaper exceeded its shutdown deadline".to_string(),
                ))
            }
        }
    }
}

impl Drop for HandLeaseReaperHandle {
    fn drop(&mut self) {
        self.state
            .exited
            .store(true, std::sync::atomic::Ordering::Release);
        self.shutdown.cancel();
        self.task.abort();
    }
}

impl HandLeaseReaperReadiness {
    /// Returns the age of the most recent complete successful sweep.
    #[must_use]
    pub fn heartbeat_age(&self) -> Duration {
        let heartbeat = self
            .state
            .last_heartbeat
            .read()
            .ok()
            .and_then(|guard| *guard);
        heartbeat.map_or_else(
            || self.state.started_at.elapsed(),
            |heartbeat| heartbeat.elapsed(),
        )
    }

    /// Returns a bounded reason readiness must refuse sandbox traffic.
    #[must_use]
    pub fn unready_reason(&self) -> Option<String> {
        if self.state.exited.load(std::sync::atomic::Ordering::Acquire) {
            return Some("hand lease reaper exited unexpectedly".to_string());
        }
        if self.heartbeat_age() > self.heartbeat_maximum_age {
            return Some("hand lease reaper heartbeat is stale".to_string());
        }
        match self.state.unready_reason.read() {
            Ok(reason) => reason.clone(),
            Err(_) => Some("hand lease reaper health lock is poisoned".to_string()),
        }
    }
}

fn set_reaper_heartbeat(state: &HandLeaseReaperHealth) -> Result<()> {
    *state.last_heartbeat.write().map_err(|_| {
        MoaError::StorageError("hand lease reaper heartbeat lock is poisoned".to_string())
    })? = Some(Instant::now());
    *state.unready_reason.write().map_err(|_| {
        MoaError::StorageError("hand lease reaper health lock is poisoned".to_string())
    })? = None;
    Ok(())
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
        let concurrency = self.config.max_destroy_concurrency.max(1);
        let claim_limit = self
            .config
            .batch_size
            .min(i64::try_from(concurrency).unwrap_or(i64::MAX))
            .max(0);
        let claimed = self
            .claims
            .claim_expired(claim_limit, self.config.claim_ttl)
            .await?;
        let outcomes = stream::iter(claimed)
            .map(|lease| async move {
                match self.destroy_claimed_with_renewal(&lease).await {
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
            .buffer_unordered(concurrency)
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
        if let Some(not_before) = lease.reconcile_not_before {
            let delay = (not_before - Utc::now()).to_std().unwrap_or(Duration::ZERO);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
        }
        destroy_provisioning_operations(
            provider.as_ref(),
            lease.provider_account_id,
            lease.provider_account_generation,
            lease.provisioning_operation_id,
            lease.handle.as_ref(),
            ProvisioningAbsenceProof::Delayed,
        )
        .await
    }

    async fn destroy_claimed_with_renewal(&self, lease: &ClaimedHandLease) -> Result<()> {
        if !self
            .claims
            .renew_claim(lease, self.config.claim_ttl)
            .await?
        {
            return Err(MoaError::StorageError(format!(
                "durable hand reaper lost claim before destroy for session {} provider {} generation {}",
                lease.session_id, lease.provider, lease.generation
            )));
        }
        let heartbeat = (self.config.claim_ttl / 3).max(Duration::from_millis(1));
        let mut ticker = tokio::time::interval(heartbeat);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        let cleanup = self.destroy_claimed(lease);
        tokio::pin!(cleanup);
        loop {
            tokio::select! {
                outcome = &mut cleanup => return outcome,
                _ = ticker.tick() => {
                    if !self.claims.renew_claim(lease, self.config.claim_ttl).await? {
                        return Err(MoaError::StorageError(format!(
                            "durable hand reaper lost claim renewal for session {} provider {} generation {}",
                            lease.session_id, lease.provider, lease.generation
                        )));
                    }
                }
            }
        }
    }

    /// Spawns the supervised sweep loop.
    pub fn spawn(self) -> Result<HandLeaseReaperHandle> {
        if self.config.interval.is_zero()
            || self.config.batch_size <= 0
            || self.config.max_destroy_concurrency == 0
            || self.config.heartbeat_maximum_age.is_zero()
            || self.config.interval >= self.config.heartbeat_maximum_age
        {
            return Err(MoaError::ConfigError(
                "hand lease reaper requires positive sweep bounds and an interval shorter than heartbeat freshness"
                    .to_string(),
            ));
        }
        let heartbeat_maximum_age = self.config.heartbeat_maximum_age;
        let state = Arc::new(HandLeaseReaperHealth {
            started_at: Instant::now(),
            last_heartbeat: RwLock::new(None),
            unready_reason: RwLock::new(Some(
                "hand lease reaper has not completed its first pass".to_string(),
            )),
            exited: std::sync::atomic::AtomicBool::new(false),
        });
        let shutdown = CancellationToken::new();
        let task_state = Arc::clone(&state);
        let task_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            let result = async {
                loop {
                    self.sweep().await?;
                    set_reaper_heartbeat(&task_state)?;
                    tokio::select! {
                        () = task_shutdown.cancelled() => return Ok(()),
                        () = tokio::time::sleep(self.config.interval) => {}
                    }
                }
            }
            .await;
            task_state
                .exited
                .store(true, std::sync::atomic::Ordering::Release);
            if result.is_err()
                && let Ok(mut reason) = task_state.unready_reason.write()
            {
                *reason = Some("hand lease reaper pass failed".to_string());
            }
            result
        });
        Ok(HandLeaseReaperHandle {
            state,
            shutdown,
            task,
            heartbeat_maximum_age,
        })
    }
}

/// Selects how provider absence is proven after destroying an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProvisioningAbsenceProof {
    /// Accepts one empty enumeration for an explicitly bound durable handle.
    Immediate,
    /// Requires two empty enumerations separated by the consistency window.
    Delayed,
}

/// Destroys and confirms every resource associated with one lease generation.
///
/// The stored handle may belong to the previous generation while the current
/// provisioning operation is replacing it, so both operation identities are
/// enumerated. Callers choose whether a known explicit teardown can accept one
/// empty enumeration or ambiguous cleanup needs delayed confirmation.
pub(super) async fn destroy_provisioning_operations(
    provider: &dyn HandProvider,
    provider_account_id: ProviderAccountId,
    provider_account_generation: u64,
    current_operation_id: HandProvisioningOperationId,
    stored_handle: Option<&LeaseHandle>,
    absence_proof: ProvisioningAbsenceProof,
) -> Result<()> {
    let mut operation_ids = vec![current_operation_id];
    if let Some(stored_handle) = stored_handle
        && stored_handle.provisioning_operation_id != current_operation_id
    {
        operation_ids.push(stored_handle.provisioning_operation_id);
    }

    let mut handles = Vec::new();
    if let Some(stored_handle) = stored_handle {
        handles.push(stored_handle.handle.clone());
    }
    let mut first_error = None;
    for operation_id in &operation_ids {
        match provider
            .provisioned_hands(
                provider_account_id,
                provider_account_generation,
                *operation_id,
            )
            .await
        {
            Ok(discovered) => {
                for handle in discovered {
                    if !handles.contains(&handle) {
                        handles.push(handle);
                    }
                }
            }
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }

    for handle in &handles {
        if let Err(error) = provider.destroy(handle).await
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }

    if let Some(error) = first_error {
        return Err(error);
    }
    ensure_provisioning_operations_absent(
        provider,
        provider_account_id,
        provider_account_generation,
        &operation_ids,
    )
    .await?;
    match absence_proof {
        ProvisioningAbsenceProof::Immediate => Ok(()),
        ProvisioningAbsenceProof::Delayed => {
            tokio::time::sleep(PROVISIONING_EMPTY_CONFIRMATION).await;
            ensure_provisioning_operations_absent(
                provider,
                provider_account_id,
                provider_account_generation,
                &operation_ids,
            )
            .await
        }
    }
}

async fn ensure_provisioning_operations_absent(
    provider: &dyn HandProvider,
    provider_account_id: ProviderAccountId,
    provider_account_generation: u64,
    operation_ids: &[HandProvisioningOperationId],
) -> Result<()> {
    for operation_id in operation_ids {
        let remaining = provider
            .provisioned_hands(
                provider_account_id,
                provider_account_generation,
                *operation_id,
            )
            .await?;
        if !remaining.is_empty() {
            return Err(MoaError::ProviderError(format!(
                "hand provider {} still reports {} resource(s) for provisioning operation {operation_id} after destroy",
                provider.provider_name(),
                remaining.len()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct RecordingClaims {
        claimed: Mutex<Vec<ClaimedHandLease>>,
        limits: Mutex<Vec<i64>>,
        finalized: Mutex<Vec<i64>>,
        released: Mutex<Vec<(i64, Duration)>>,
    }

    #[async_trait::async_trait]
    impl ExpiredHandLeaseClaims for RecordingClaims {
        async fn claim_expired(
            &self,
            limit: i64,
            _claim_ttl: Duration,
        ) -> Result<Vec<ClaimedHandLease>> {
            self.limits.lock().expect("limits lock").push(limit);
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

        async fn renew_claim(
            &self,
            _claimed: &ClaimedHandLease,
            _claim_ttl: Duration,
        ) -> Result<bool> {
            Ok(true)
        }
    }

    struct StubProvider {
        destroy_fails: bool,
        panic_on_reconcile: bool,
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

        async fn provisioned_hands(
            &self,
            _provider_account_id: ProviderAccountId,
            _provider_account_generation: u64,
            _operation_id: HandProvisioningOperationId,
        ) -> Result<Vec<HandHandle>> {
            assert!(
                !self.panic_on_reconcile,
                "a stale reaper claim must be fenced before any provider reconciliation"
            );
            Ok(Vec::new())
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
        let provisioning_operation_id = HandProvisioningOperationId::new();
        ClaimedHandLease {
            tenant_id: TenantId::new(),
            session_id: SessionId::new(),
            worker_id: String::new(),
            provider: "local".to_string(),
            provider_account_id: ProviderAccountId(Uuid::nil()),
            provider_account_generation: 0,
            generation,
            provisioning_operation_id,
            claim_token: Uuid::new_v4(),
            handle: Some(LeaseHandle::new(
                provisioning_operation_id,
                HandHandle::local(std::path::PathBuf::from("/tmp/moa-reap")),
            )),
            attachment: Some(
                HandLeaseWorkspaceAttachment::new(SandboxWorkspaceId::new(), 1, 1, None)
                    .expect("test attachment validates"),
            ),
            reconcile_not_before: None,
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
                panic_on_reconcile: false,
            })],
            HandLeaseReaperConfig::default(),
        );

        assert_eq!(reaper.sweep().await.expect("sweep"), 1);
        assert_eq!(
            *claims.limits.lock().expect("limits lock"),
            vec![4],
            "one sweep must not claim more rows than it can heartbeat concurrently"
        );
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
                panic_on_reconcile: false,
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

    #[tokio::test]
    async fn lost_attachment_claim_is_fenced_before_provider_destruction() {
        // Pins: the reaper re-validates its exact durable claim immediately
        // before provider I/O, so a claim that lost its attachment fence cannot
        // destroy compute belonging to a retained or replacement attachment.
        struct LostClaim;

        #[async_trait::async_trait]
        impl ExpiredHandLeaseClaims for LostClaim {
            async fn claim_expired(
                &self,
                _limit: i64,
                _claim_ttl: Duration,
            ) -> Result<Vec<ClaimedHandLease>> {
                Ok(vec![claimed(11, 0)])
            }

            async fn finalize_destroyed(&self, _claimed: &ClaimedHandLease) -> Result<bool> {
                panic!("a lost claim must not finalize")
            }

            async fn release_for_retry(
                &self,
                _claimed: &ClaimedHandLease,
                _retry_after: Duration,
            ) -> Result<bool> {
                Ok(false)
            }

            async fn renew_claim(
                &self,
                _claimed: &ClaimedHandLease,
                _claim_ttl: Duration,
            ) -> Result<bool> {
                Ok(false)
            }
        }

        let reaper = HandLeaseReaper::new(
            Arc::new(LostClaim),
            vec![Arc::new(StubProvider {
                destroy_fails: false,
                panic_on_reconcile: true,
            })],
            HandLeaseReaperConfig::default(),
        );

        assert_eq!(
            reaper.sweep().await.expect("lost claim is a fenced no-op"),
            0
        );
    }

    #[tokio::test]
    async fn supervised_reaper_opens_readiness_after_a_complete_pass_and_shuts_down() {
        // Pins: sandbox traffic stays unready until the destruction owner has
        // completed a full database pass, after which a graceful shutdown joins
        // the owner instead of abandoning it.
        let config = HandLeaseReaperConfig {
            interval: Duration::from_secs(60),
            heartbeat_maximum_age: Duration::from_secs(120),
            ..HandLeaseReaperConfig::default()
        };
        let handle = HandLeaseReaper::new(Arc::new(RecordingClaims::default()), Vec::new(), config)
            .spawn()
            .expect("valid supervised reaper config should start");
        let readiness = handle.readiness();

        tokio::time::timeout(Duration::from_secs(1), async {
            while readiness.unready_reason().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first empty sweep should open readiness");
        assert!(
            handle.heartbeat_age() < Duration::from_secs(1),
            "the first complete sweep should record a fresh heartbeat"
        );
        handle
            .shutdown()
            .await
            .expect("graceful shutdown should join the reaper");
    }

    #[tokio::test]
    async fn supervised_reaper_failure_closes_readiness_and_surfaces_task_result() {
        // Pins: a failed cleanup pass cannot leave the process reporting ready;
        // the task result reaches the process supervisor as a fatal error.
        struct FailingClaims;

        #[async_trait::async_trait]
        impl ExpiredHandLeaseClaims for FailingClaims {
            async fn claim_expired(
                &self,
                _limit: i64,
                _claim_ttl: Duration,
            ) -> Result<Vec<ClaimedHandLease>> {
                Err(MoaError::StorageError("forced reaper failure".to_string()))
            }

            async fn finalize_destroyed(&self, _claimed: &ClaimedHandLease) -> Result<bool> {
                panic!("a failed claim pass cannot finalize work")
            }

            async fn release_for_retry(
                &self,
                _claimed: &ClaimedHandLease,
                _retry_after: Duration,
            ) -> Result<bool> {
                panic!("a failed claim pass cannot release work")
            }

            async fn renew_claim(
                &self,
                _claimed: &ClaimedHandLease,
                _claim_ttl: Duration,
            ) -> Result<bool> {
                panic!("a failed claim pass cannot renew work")
            }
        }

        let config = HandLeaseReaperConfig {
            interval: Duration::from_millis(10),
            heartbeat_maximum_age: Duration::from_secs(1),
            ..HandLeaseReaperConfig::default()
        };
        let mut handle = HandLeaseReaper::new(Arc::new(FailingClaims), Vec::new(), config)
            .spawn()
            .expect("valid supervised reaper config should start");
        let readiness = handle.readiness();

        let error = handle
            .task_result()
            .await
            .expect_err("failed sweep must terminate the supervised owner");
        assert!(
            matches!(error, MoaError::StorageError(message) if message == "forced reaper failure")
        );
        assert_eq!(
            readiness.unready_reason().as_deref(),
            Some("hand lease reaper exited unexpectedly")
        );
    }

    #[tokio::test]
    async fn dropping_supervised_reaper_immediately_closes_readiness() {
        // Pins: losing the cleanup-owner handle cannot leave a cloned process
        // readiness projection healthy until its heartbeat eventually ages out.
        let config = HandLeaseReaperConfig {
            interval: Duration::from_secs(60),
            heartbeat_maximum_age: Duration::from_secs(120),
            ..HandLeaseReaperConfig::default()
        };
        let handle = HandLeaseReaper::new(Arc::new(RecordingClaims::default()), Vec::new(), config)
            .spawn()
            .expect("valid supervised reaper config should start");
        let readiness = handle.readiness();
        drop(handle);

        assert_eq!(
            readiness.unready_reason().as_deref(),
            Some("hand lease reaper exited unexpectedly")
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
    fn claim_decoder_rejects_partial_workspace_attachments() {
        // Pins: the reaper never turns a partial durable row into a claim with
        // invented fence values; legacy absence is all-null and attachment is
        // otherwise all-or-none.
        let error = claimed_attachment(Some(SandboxWorkspaceId::new()), None, Some(1), None)
            .expect_err("partial attachment must fail closed");
        assert!(
            matches!(error, MoaError::StorageError(message) if message ==
            "reaper claim contains a partial durable workspace attachment")
        );

        assert_eq!(
            claimed_attachment(None, None, None, None)
                .expect("all-null legacy attachment is valid"),
            None
        );
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

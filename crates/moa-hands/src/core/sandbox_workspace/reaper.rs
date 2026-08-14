//! Independent bounded cleanup and reconciliation for durable workspaces.

use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use futures_util::{StreamExt, stream};
use moa_core::error::{MoaError, Result};
use moa_core::types::sandbox_workspace::{
    WorkspaceBinding, WorkspaceCheckpointPublication, WorkspaceOperationKind,
    WorkspacePostCommitState,
};

use super::operations::{
    AbsenceObservation, ClaimedWorkspaceOperation, PostgresWorkspaceOperationRepository,
};
use super::{
    checkpoint::model::PublishCheckpointCommitRequest,
    maintenance::WorkspaceMaintenanceCoordinator, repository::PostgresWorkspaceRepository,
};
use crate::core::{leases::HandLease, telemetry::record_workspace_reaper_health};
use tokio::{task::JoinHandle, time::Instant};
use tokio_util::sync::CancellationToken;

/// One provider inventory observation for an exact fenced operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceInventoryObservation {
    /// The exact MOA-owned provider resource exists.
    Present {
        /// Stable digest of the verified provider inventory.
        inventory_digest: String,
    },
    /// A complete portable-checkpoint publication is ready for atomic recovery.
    CheckpointPublication {
        /// Stable digest of the verified provider publication evidence.
        inventory_digest: String,
        /// Exact durable workspace binding reconstructed under maintenance authority.
        binding: Box<WorkspaceBinding>,
        /// Complete manifest-backed portable checkpoint publication.
        publication: Box<WorkspaceCheckpointPublication>,
        /// Verified provider-selected compute disposition.
        post_commit_state: WorkspacePostCommitState,
        /// Exact active lease generation that held the writer.
        lease: Box<HandLease>,
    },
    /// The verified provider inventory is empty for this operation.
    Empty {
        /// Stable digest of the empty inventory response and ownership filter.
        inventory_digest: String,
    },
}

/// Provider-specific inventory probe used only by the workspace reaper.
#[async_trait]
pub trait WorkspaceReconciliationProbe: Send + Sync {
    /// Observes the exact provider account, workspace, operation, and generations.
    async fn observe(
        &self,
        claimed: &ClaimedWorkspaceOperation,
    ) -> Result<WorkspaceInventoryObservation>;
}

/// Result counts from one bounded workspace-reaper pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkspaceReaperPass {
    /// Operations claimed by this replica.
    pub claimed: usize,
    /// Operations confirmed with a present resource.
    pub confirmed_present: usize,
    /// Operations confirmed absent after two separated observations.
    pub confirmed_absent: usize,
    /// Operations waiting for the second empty observation.
    pub awaiting_second_empty: usize,
    /// Operations released behind failure backoff.
    pub retrying: usize,
}

/// Independent maintenance cadences for safety and resource-intensive workspace work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceReaperCadenceConfig {
    /// Prompt cadence for due/backlog reconciliation and readiness heartbeat.
    pub safety_interval: Duration,
    /// Initial checkpoint-retention cadence after work or failure.
    pub retention_initial_interval: Duration,
    /// Maximum checkpoint-retention cadence after consecutive idle passes.
    pub retention_maximum_interval: Duration,
    /// Initial full provider-inventory cadence after work or failure.
    pub inventory_initial_interval: Duration,
    /// Maximum full provider-inventory cadence after consecutive idle passes.
    pub inventory_maximum_interval: Duration,
    /// Fixed low-frequency fleet metric refresh cadence.
    pub fleet_metrics_interval: Duration,
}

impl WorkspaceReaperCadenceConfig {
    fn validate(self, heartbeat_maximum_age: Duration) -> Result<()> {
        if self.safety_interval.is_zero()
            || self.retention_initial_interval.is_zero()
            || self.retention_maximum_interval < self.retention_initial_interval
            || self.inventory_initial_interval.is_zero()
            || self.inventory_maximum_interval < self.inventory_initial_interval
            || self.fleet_metrics_interval.is_zero()
            || heartbeat_maximum_age.is_zero()
            || self.safety_interval >= heartbeat_maximum_age
        {
            return Err(MoaError::ConfigError(
                "workspace reaper requires positive ordered cadences and a safety interval shorter than heartbeat freshness"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

/// Cross-replica workspace cleanup owner.
pub struct WorkspaceReaper {
    operations: Arc<PostgresWorkspaceOperationRepository>,
    workspaces: Arc<PostgresWorkspaceRepository>,
    probe: Arc<dyn WorkspaceReconciliationProbe>,
    claim_ttl: Duration,
    max_concurrency: usize,
}

/// Supervised process handle for workspace reconciliation and maintenance.
pub struct WorkspaceReaperHandle {
    state: Arc<WorkspaceReaperHealth>,
    shutdown: CancellationToken,
    task: JoinHandle<Result<()>>,
    sampler: JoinHandle<()>,
    heartbeat_maximum_age: Duration,
}

/// Cloneable readiness projection for the supervised workspace reaper.
#[derive(Clone)]
pub struct WorkspaceReaperReadiness {
    state: Arc<WorkspaceReaperHealth>,
    heartbeat_maximum_age: Duration,
}

#[derive(Debug)]
struct WorkspaceReaperHealth {
    started_at: Instant,
    last_heartbeat: RwLock<Option<Instant>>,
    backlog: std::sync::atomic::AtomicU64,
    oldest_work_seconds: std::sync::atomic::AtomicU64,
    unready_reason: RwLock<Option<String>>,
    exited: std::sync::atomic::AtomicBool,
}

#[derive(Debug, Clone, Copy)]
struct AdaptiveCadence {
    initial: Duration,
    maximum: Duration,
    current: Duration,
    idle_streak: u32,
}

impl AdaptiveCadence {
    fn new(initial: Duration, maximum: Duration) -> Self {
        Self {
            initial,
            maximum,
            current: initial,
            idle_streak: 0,
        }
    }

    fn record_success(&mut self, idle: bool) {
        if idle {
            self.idle_streak = self.idle_streak.saturating_add(1);
            self.current = self.current.saturating_mul(2).min(self.maximum);
        } else {
            self.reset();
        }
    }

    fn reset(&mut self) {
        self.current = self.initial;
        self.idle_streak = 0;
    }
}

impl WorkspaceReaperHandle {
    /// Starts the supervised reaper before listener readiness.
    pub fn spawn(
        coordinator: Arc<WorkspaceMaintenanceCoordinator>,
        reaper: WorkspaceReaper,
        cadences: WorkspaceReaperCadenceConfig,
        batch_size: i64,
        heartbeat_maximum_age: Duration,
    ) -> Result<Self> {
        cadences.validate(heartbeat_maximum_age)?;
        if batch_size <= 0 {
            return Err(MoaError::ConfigError(
                "workspace reaper requires a positive reconciliation batch".to_string(),
            ));
        }
        let state = Arc::new(WorkspaceReaperHealth {
            started_at: Instant::now(),
            last_heartbeat: RwLock::new(None),
            backlog: std::sync::atomic::AtomicU64::new(0),
            oldest_work_seconds: std::sync::atomic::AtomicU64::new(0),
            unready_reason: RwLock::new(Some(
                "workspace reaper has not completed its first pass".to_string(),
            )),
            exited: std::sync::atomic::AtomicBool::new(false),
        });
        let shutdown = CancellationToken::new();
        let task_state = Arc::clone(&state);
        let task_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            let result = supervise_workspace_maintenance(
                coordinator,
                reaper,
                cadences,
                batch_size,
                Arc::clone(&task_state),
                task_shutdown,
            )
            .await;
            task_state
                .exited
                .store(true, std::sync::atomic::Ordering::Release);
            if result.is_err()
                && let Ok(mut reason) = task_state.unready_reason.write()
            {
                *reason = Some("workspace reaper pass failed".to_string());
            }
            if result.is_err() {
                let heartbeat_age = task_state
                    .last_heartbeat
                    .read()
                    .ok()
                    .and_then(|heartbeat| *heartbeat)
                    .map_or_else(
                        || task_state.started_at.elapsed(),
                        |heartbeat| heartbeat.elapsed(),
                    );
                record_workspace_reaper_health(
                    false,
                    heartbeat_age,
                    task_state
                        .backlog
                        .load(std::sync::atomic::Ordering::Acquire),
                    Duration::from_secs(
                        task_state
                            .oldest_work_seconds
                            .load(std::sync::atomic::Ordering::Acquire),
                    ),
                );
            }
            result
        });
        let sampler_state = Arc::clone(&state);
        let sampler_shutdown = shutdown.clone();
        let sampler = tokio::spawn(async move {
            let readiness = WorkspaceReaperReadiness {
                state: sampler_state,
                heartbeat_maximum_age,
            };
            loop {
                let _ = readiness.unready_reason();
                tokio::select! {
                    () = sampler_shutdown.cancelled() => return,
                    () = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
            }
        });
        Ok(Self {
            state,
            shutdown,
            task,
            sampler,
            heartbeat_maximum_age,
        })
    }

    /// Returns the age of the most recent successful complete maintenance pass.
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

    /// Returns the last durable backlog count sampled by the supervised loop.
    #[must_use]
    pub fn backlog(&self) -> u64 {
        self.state
            .backlog
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Returns the sampled age of the oldest outstanding operation.
    #[must_use]
    pub fn oldest_work_age(&self) -> Duration {
        Duration::from_secs(
            self.state
                .oldest_work_seconds
                .load(std::sync::atomic::Ordering::Acquire),
        )
    }

    /// Returns a bounded reason readiness must refuse workspace traffic.
    #[must_use]
    pub fn unready_reason(&self) -> Option<String> {
        self.readiness().unready_reason()
    }

    /// Returns a cloneable health projection for process readiness.
    #[must_use]
    pub fn readiness(&self) -> WorkspaceReaperReadiness {
        WorkspaceReaperReadiness {
            state: Arc::clone(&self.state),
            heartbeat_maximum_age: self.heartbeat_maximum_age,
        }
    }

    /// Awaits the task result so unexpected exit can be process-fatal.
    pub async fn task_result(&mut self) -> Result<()> {
        match (&mut self.task).await {
            Ok(result) => result,
            Err(error) => Err(MoaError::StorageError(format!(
                "workspace reaper task join failed: {error}"
            ))),
        }
    }

    /// Cancels and joins the supervised task during graceful shutdown.
    pub async fn shutdown(mut self) -> Result<()> {
        self.shutdown.cancel();
        let task_result = match tokio::time::timeout(Duration::from_secs(10), &mut self.task).await
        {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => Err(MoaError::StorageError(format!(
                "workspace reaper task join failed: {error}"
            ))),
            Err(_) => {
                self.task.abort();
                let _ = (&mut self.task).await;
                Err(MoaError::StorageError(
                    "workspace reaper exceeded its shutdown deadline".to_string(),
                ))
            }
        };
        if tokio::time::timeout(Duration::from_secs(1), &mut self.sampler)
            .await
            .is_err()
        {
            self.sampler.abort();
            let _ = (&mut self.sampler).await;
        }
        task_result
    }
}

impl Drop for WorkspaceReaperHandle {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.task.abort();
        self.sampler.abort();
    }
}

impl WorkspaceReaperReadiness {
    /// Returns the age of the most recent complete pass.
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

    /// Returns a bounded readiness failure reason, when any.
    #[must_use]
    pub fn unready_reason(&self) -> Option<String> {
        let heartbeat_age = self.heartbeat_age();
        let reason = if self.state.exited.load(std::sync::atomic::Ordering::Acquire) {
            Some("workspace reaper exited unexpectedly".to_string())
        } else if heartbeat_age > self.heartbeat_maximum_age {
            Some("workspace reaper heartbeat is stale".to_string())
        } else {
            self.state
                .unready_reason
                .read()
                .ok()
                .and_then(|reason| reason.clone())
        };
        record_workspace_reaper_health(
            reason.is_none(),
            heartbeat_age,
            self.state
                .backlog
                .load(std::sync::atomic::Ordering::Acquire),
            Duration::from_secs(
                self.state
                    .oldest_work_seconds
                    .load(std::sync::atomic::Ordering::Acquire),
            ),
        );
        reason
    }
}

async fn supervise_workspace_maintenance(
    coordinator: Arc<WorkspaceMaintenanceCoordinator>,
    reaper: WorkspaceReaper,
    cadences: WorkspaceReaperCadenceConfig,
    batch_size: i64,
    state: Arc<WorkspaceReaperHealth>,
    shutdown: CancellationToken,
) -> Result<()> {
    let mut lanes = tokio::task::JoinSet::new();
    lanes.spawn(run_safety_lane(
        Arc::clone(&coordinator),
        reaper,
        cadences.safety_interval,
        batch_size,
        Arc::clone(&state),
        shutdown.clone(),
    ));
    lanes.spawn(run_retention_lane(
        Arc::clone(&coordinator),
        AdaptiveCadence::new(
            cadences.retention_initial_interval,
            cadences.retention_maximum_interval,
        ),
        shutdown.clone(),
    ));
    lanes.spawn(run_inventory_lane(
        Arc::clone(&coordinator),
        AdaptiveCadence::new(
            cadences.inventory_initial_interval,
            cadences.inventory_maximum_interval,
        ),
        usize::try_from(batch_size).map_err(|_| {
            MoaError::ConfigError(
                "workspace inventory reconciliation batch exceeds this platform".to_string(),
            )
        })?,
        shutdown.clone(),
    ));
    lanes.spawn(run_metrics_lane(
        coordinator,
        cadences.fleet_metrics_interval,
        shutdown.clone(),
    ));

    let mut failure = None;
    while let Some(joined) = lanes.join_next().await {
        match joined {
            Ok(Ok(())) if shutdown.is_cancelled() => {}
            Ok(Ok(())) => {
                failure = Some(MoaError::StorageError(
                    "workspace maintenance lane exited unexpectedly".to_string(),
                ));
                shutdown.cancel();
            }
            Ok(Err(error)) => {
                failure = Some(error);
                shutdown.cancel();
            }
            Err(error) => {
                failure = Some(MoaError::StorageError(format!(
                    "workspace maintenance lane join failed: {error}"
                )));
                shutdown.cancel();
            }
        }
    }
    failure.map_or(Ok(()), Err)
}

async fn run_safety_lane(
    coordinator: Arc<WorkspaceMaintenanceCoordinator>,
    reaper: WorkspaceReaper,
    interval: Duration,
    batch_size: i64,
    state: Arc<WorkspaceReaperHealth>,
    shutdown: CancellationToken,
) -> Result<()> {
    loop {
        if shutdown.is_cancelled() {
            return Ok(());
        }
        let backlog = coordinator.backlog().await?;
        state
            .backlog
            .store(backlog.count, std::sync::atomic::Ordering::Release);
        state.oldest_work_seconds.store(
            backlog.oldest_age.as_secs(),
            std::sync::atomic::Ordering::Release,
        );
        reaper.run_once(batch_size).await?;
        set_reaper_heartbeat(&state)?;
        record_workspace_reaper_health(true, Duration::ZERO, backlog.count, backlog.oldest_age);
        if wait_for_lane(&shutdown, interval).await {
            return Ok(());
        }
    }
}

async fn run_retention_lane(
    coordinator: Arc<WorkspaceMaintenanceCoordinator>,
    mut cadence: AdaptiveCadence,
    shutdown: CancellationToken,
) -> Result<()> {
    loop {
        if shutdown.is_cancelled() {
            return Ok(());
        }
        match coordinator.run_retention_once().await {
            Ok(pass) => {
                let idle = pass.claimed == 0
                    && pass.deleted == 0
                    && pass.awaiting_absence == 0
                    && pass.retrying == 0;
                cadence.record_success(idle);
            }
            Err(error) => {
                cadence.reset();
                tracing::warn!(
                    error_code = safe_error_code(&error),
                    "workspace checkpoint retention pass failed and will retry"
                );
            }
        }
        if wait_for_lane(&shutdown, cadence.current).await {
            return Ok(());
        }
    }
}

async fn run_inventory_lane(
    coordinator: Arc<WorkspaceMaintenanceCoordinator>,
    mut cadence: AdaptiveCadence,
    batch_size: usize,
    shutdown: CancellationToken,
) -> Result<()> {
    loop {
        if shutdown.is_cancelled() {
            return Ok(());
        }
        match coordinator
            .reconcile_claimed_provider_inventory_once(batch_size)
            .await
        {
            Ok(pass) => {
                let idle = pass.resources == 0 && pass.unresolved_findings == 0;
                cadence.record_success(idle);
            }
            Err(error) => {
                cadence.reset();
                tracing::warn!(
                    error_code = safe_error_code(&error),
                    "workspace provider inventory pass failed and will retry"
                );
            }
        }
        if wait_for_lane(&shutdown, cadence.current).await {
            return Ok(());
        }
    }
}

async fn run_metrics_lane(
    coordinator: Arc<WorkspaceMaintenanceCoordinator>,
    interval: Duration,
    shutdown: CancellationToken,
) -> Result<()> {
    loop {
        if shutdown.is_cancelled() {
            return Ok(());
        }
        if let Err(error) = coordinator.emit_fleet_metrics().await {
            tracing::warn!(
                error_code = safe_error_code(&error),
                "workspace fleet metric refresh failed and will retry"
            );
        }
        if wait_for_lane(&shutdown, interval).await {
            return Ok(());
        }
    }
}

async fn wait_for_lane(shutdown: &CancellationToken, delay: Duration) -> bool {
    tokio::select! {
        () = shutdown.cancelled() => true,
        () = tokio::time::sleep(delay) => false,
    }
}

fn set_reaper_heartbeat(state: &WorkspaceReaperHealth) -> Result<()> {
    *state.last_heartbeat.write().map_err(|_| {
        MoaError::StorageError("workspace reaper heartbeat lock is poisoned".to_string())
    })? = Some(Instant::now());
    *state.unready_reason.write().map_err(|_| {
        MoaError::StorageError("workspace reaper health lock is poisoned".to_string())
    })? = None;
    Ok(())
}

impl WorkspaceReaper {
    /// Creates a reaper with positive claim and concurrency bounds.
    pub fn new(
        operations: Arc<PostgresWorkspaceOperationRepository>,
        workspaces: Arc<PostgresWorkspaceRepository>,
        probe: Arc<dyn WorkspaceReconciliationProbe>,
        claim_ttl: Duration,
        max_concurrency: usize,
    ) -> Result<Self> {
        if claim_ttl.is_zero() || max_concurrency == 0 {
            return Err(MoaError::ValidationError(
                "workspace reaper requires positive claim ttl and concurrency".to_string(),
            ));
        }
        Ok(Self {
            operations,
            workspaces,
            probe,
            claim_ttl,
            max_concurrency,
        })
    }

    /// Reconciles one disjoint, bounded batch without sharing compute-hand ownership.
    pub async fn run_once(&self, limit: i64) -> Result<WorkspaceReaperPass> {
        let claimed = self
            .operations
            .claim_reconciliation(limit, self.claim_ttl)
            .await?;
        let claimed_count = claimed.len();
        let results = stream::iter(
            claimed
                .into_iter()
                .map(|claimed| async move { self.reconcile_one(claimed).await }),
        )
        .buffer_unordered(self.max_concurrency)
        .collect::<Vec<_>>()
        .await;

        let mut pass = WorkspaceReaperPass {
            claimed: claimed_count,
            ..WorkspaceReaperPass::default()
        };
        for result in results {
            match result? {
                ReconcileOutcome::Present => pass.confirmed_present += 1,
                ReconcileOutcome::Absent => pass.confirmed_absent += 1,
                ReconcileOutcome::AwaitingSecondEmpty => pass.awaiting_second_empty += 1,
                ReconcileOutcome::Retrying => pass.retrying += 1,
            }
        }
        Ok(pass)
    }

    async fn reconcile_one(&self, claimed: ClaimedWorkspaceOperation) -> Result<ReconcileOutcome> {
        if !self
            .operations
            .renew_claim(&claimed, self.claim_ttl)
            .await?
        {
            return Ok(ReconcileOutcome::Retrying);
        }
        match self.probe.observe(&claimed).await {
            Ok(WorkspaceInventoryObservation::CheckpointPublication {
                inventory_digest,
                binding,
                publication,
                post_commit_state,
                lease,
            }) => {
                self.operations
                    .record_inventory_observation(&claimed, false, &inventory_digest, Utc::now())
                    .await?;
                let request = PublishCheckpointCommitRequest {
                    binding: &binding,
                    operation_id: claimed.operation.operation_id,
                    publication: &publication,
                    post_commit_state,
                    lease: &lease,
                };
                if self
                    .workspaces
                    .publish_checkpoint_commit_claimed(request, &claimed)
                    .await?
                {
                    Ok(ReconcileOutcome::Present)
                } else {
                    Ok(ReconcileOutcome::Retrying)
                }
            }
            Ok(WorkspaceInventoryObservation::Present { inventory_digest }) => {
                self.operations
                    .record_inventory_observation(&claimed, false, &inventory_digest, Utc::now())
                    .await?;
                if matches!(
                    claimed.operation.kind,
                    WorkspaceOperationKind::Commit
                        | WorkspaceOperationKind::Checkpoint
                        | WorkspaceOperationKind::Delete
                ) {
                    self.operations
                        .release_claim(
                            &claimed,
                            reconciliation_backoff(claimed.operation.attempts),
                            if matches!(
                                claimed.operation.kind,
                                WorkspaceOperationKind::Commit | WorkspaceOperationKind::Checkpoint
                            ) {
                                "checkpoint_publication_incomplete"
                            } else {
                                "resource_still_present"
                            },
                        )
                        .await?;
                    return Ok(ReconcileOutcome::Retrying);
                }
                if self.operations.confirm_present_claimed(&claimed).await? {
                    Ok(ReconcileOutcome::Present)
                } else {
                    Ok(ReconcileOutcome::Retrying)
                }
            }
            Ok(WorkspaceInventoryObservation::Empty { inventory_digest }) => {
                match self
                    .operations
                    .record_inventory_observation(&claimed, true, &inventory_digest, Utc::now())
                    .await?
                {
                    AbsenceObservation::Proven => {
                        if self.operations.confirm_absent(&claimed).await? {
                            Ok(ReconcileOutcome::Absent)
                        } else {
                            Ok(ReconcileOutcome::Retrying)
                        }
                    }
                    AbsenceObservation::First => {
                        self.operations.release_after_first_empty(&claimed).await?;
                        Ok(ReconcileOutcome::AwaitingSecondEmpty)
                    }
                    AbsenceObservation::Reset => Err(MoaError::StorageError(
                        "empty provider inventory unexpectedly reset its absence proof".to_string(),
                    )),
                }
            }
            Err(error) => {
                let retry = reconciliation_backoff(claimed.operation.attempts);
                let code = safe_error_code(&error);
                self.operations.release_claim(&claimed, retry, code).await?;
                tracing::warn!(
                    tenant_id = %claimed.operation.tenant_id,
                    workspace_id = %claimed.operation.workspace_id,
                    operation_id = %claimed.operation.operation_id,
                    error_code = code,
                    "workspace reconciliation probe failed"
                );
                Ok(ReconcileOutcome::Retrying)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconcileOutcome {
    Present,
    Absent,
    AwaitingSecondEmpty,
    Retrying,
}

fn reconciliation_backoff(attempts: i32) -> Duration {
    let exponent = u32::try_from(attempts.max(0)).unwrap_or(0).min(8);
    Duration::from_secs(1_u64.checked_shl(exponent).unwrap_or(256).min(300))
}

fn safe_error_code(error: &MoaError) -> &'static str {
    match error {
        MoaError::ProviderTimeout(_) => "provider_timeout",
        MoaError::ProviderTransport(_) => "provider_transport",
        MoaError::ProviderError(_) => "provider_error",
        _ => "reconciliation_error",
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, RwLock},
        time::Duration,
    };
    use tokio::time::Instant;

    use super::{
        AdaptiveCadence, WorkspaceReaperCadenceConfig, WorkspaceReaperHealth,
        WorkspaceReaperReadiness, reconciliation_backoff, set_reaper_heartbeat,
    };

    #[test]
    fn reconciliation_backoff_is_bounded_offline() {
        // Pins: repeated provider outages cannot create a hot cleanup loop or overflow.
        assert_eq!(reconciliation_backoff(0), Duration::from_secs(1));
        assert_eq!(reconciliation_backoff(8), Duration::from_secs(256));
        assert_eq!(reconciliation_backoff(i32::MAX), Duration::from_secs(256));
    }

    #[test]
    fn readiness_requires_a_complete_fresh_pass_and_rejects_exit_offline() {
        // Pins: listener readiness cannot become healthy until the complete
        // maintenance loop has succeeded, and an exited owner is always unready.
        let state = Arc::new(WorkspaceReaperHealth {
            started_at: Instant::now(),
            last_heartbeat: RwLock::new(None),
            backlog: std::sync::atomic::AtomicU64::new(0),
            oldest_work_seconds: std::sync::atomic::AtomicU64::new(0),
            unready_reason: RwLock::new(Some("first pass pending".to_string())),
            exited: std::sync::atomic::AtomicBool::new(false),
        });
        let readiness = WorkspaceReaperReadiness {
            state: Arc::clone(&state),
            heartbeat_maximum_age: Duration::from_secs(30),
        };

        assert_eq!(
            readiness.unready_reason().as_deref(),
            Some("first pass pending")
        );
        set_reaper_heartbeat(&state).expect("record complete maintenance pass");
        assert_eq!(readiness.unready_reason(), None);
        state
            .exited
            .store(true, std::sync::atomic::Ordering::Release);
        assert_eq!(
            readiness.unready_reason().as_deref(),
            Some("workspace reaper exited unexpectedly")
        );
    }

    #[test]
    fn adaptive_cadence_backs_off_only_on_idle_and_resets_on_work_or_failure_offline() {
        // Pins: empty expensive passes exponentially reduce provider/database load, while
        // discovered work and pass failures both restore the prompt initial retry cadence.
        let mut cadence = AdaptiveCadence::new(Duration::from_secs(10), Duration::from_secs(80));
        cadence.record_success(true);
        assert_eq!(
            (cadence.current, cadence.idle_streak),
            (Duration::from_secs(20), 1)
        );
        cadence.record_success(true);
        cadence.record_success(true);
        cadence.record_success(true);
        assert_eq!(
            (cadence.current, cadence.idle_streak),
            (Duration::from_secs(80), 4)
        );
        cadence.record_success(true);
        assert_eq!(
            (cadence.current, cadence.idle_streak),
            (Duration::from_secs(80), 5)
        );
        cadence.record_success(false);
        assert_eq!(
            (cadence.current, cadence.idle_streak),
            (Duration::from_secs(10), 0)
        );
        cadence.record_success(true);
        cadence.reset();
        assert_eq!(
            (cadence.current, cadence.idle_streak),
            (Duration::from_secs(10), 0)
        );
    }

    #[test]
    fn cadence_config_rejects_hot_or_inverted_bounds_offline() {
        // Pins: construction cannot accidentally enable a zero-delay hot loop or allow the
        // safety heartbeat cadence to be slower than readiness freshness.
        let valid = WorkspaceReaperCadenceConfig {
            safety_interval: Duration::from_secs(5),
            retention_initial_interval: Duration::from_secs(30),
            retention_maximum_interval: Duration::from_secs(300),
            inventory_initial_interval: Duration::from_secs(60),
            inventory_maximum_interval: Duration::from_secs(600),
            fleet_metrics_interval: Duration::from_secs(15),
        };
        valid
            .validate(Duration::from_secs(20))
            .expect("ordered nonzero cadences validate");
        assert!(
            WorkspaceReaperCadenceConfig {
                retention_maximum_interval: Duration::from_secs(1),
                ..valid
            }
            .validate(Duration::from_secs(20))
            .is_err()
        );
        assert!(valid.validate(Duration::from_secs(5)).is_err());
        assert!(
            WorkspaceReaperCadenceConfig {
                inventory_initial_interval: Duration::ZERO,
                ..valid
            }
            .validate(Duration::from_secs(20))
            .is_err()
        );
    }
}

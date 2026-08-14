//! Deterministic loopback provider for durable asynchronous execution-tool tests.

use super::*;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use moa_core::types::tools::{
    AsyncToolJob, AsyncToolJobCallbackOutcome, AsyncToolJobCancelOutcome, ExternalJobStartContext,
};
use moa_execution::wire::{
    ExecutionExternalJobCancelRequest, ExecutionExternalJobReconcileRequest,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, oneshot};
use tokio::task::JoinHandle;

/// Stable provider key registered by the provider-override orchestrator runtime.
pub const FIXTURE_EXTERNAL_JOB_PROVIDER: &str = "fixture-external-job";
/// Stable callback credential accepted only by the deterministic fixture adapter.
pub const FIXTURE_EXTERNAL_JOB_CALLBACK_TOKEN: &str = "fixture-callback-token";

/// One provider start observed after MOA durably reserved its external-job identity.
#[derive(Clone, Debug, PartialEq)]
pub struct FixtureExternalJobStart {
    /// Reserved identity, adapter key, and deterministic provider idempotency key.
    pub context: ExternalJobStartContext,
    /// Governed tool-call payload received by the provider adapter.
    pub call: serde_json::Value,
    /// Stable provider job identity committed for this reservation.
    pub provider_job_id: String,
    /// One-based order among unique provider starts.
    pub arrival_order: u64,
}

/// One provider start-recovery lookup observed after an unbound intent became due.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FixtureExternalJobRecovery {
    /// Exact reserved context reused by recovery.
    pub context: ExternalJobStartContext,
    /// One-based order among recovery requests.
    pub arrival_order: u64,
}

/// One post-bind barrier reached before the owning attempt releases active compute.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FixtureExternalJobAfterBind {
    /// Exact reserved provider context whose job has already been bound in PostgreSQL.
    pub context: ExternalJobStartContext,
    /// One-based order among unique post-bind barriers.
    pub arrival_order: u64,
}

/// One bounded sparse-reconciliation request received by the provider fixture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FixtureExternalJobReconciliation {
    /// Exact generation-fenced durable request.
    pub request: ExecutionExternalJobReconcileRequest,
    /// One-based order among reconciliation requests.
    pub arrival_order: u64,
}

/// One generation-fenced provider cancellation request received by the fixture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FixtureExternalJobCancellation {
    /// Exact generation-fenced durable request.
    pub request: ExecutionExternalJobCancelRequest,
    /// One-based order among cancellation requests.
    pub arrival_order: u64,
}

/// Controller for observing and deterministically releasing asynchronous provider operations.
#[derive(Clone)]
pub struct FixtureExternalJobController {
    state: Arc<FixtureExternalJobState>,
}

impl FixtureExternalJobController {
    /// Waits for at least `count` unique provider starts.
    pub async fn wait_for_starts(
        &self,
        count: usize,
        timeout: Duration,
    ) -> Result<Vec<FixtureExternalJobStart>> {
        wait_for_observations(
            &self.state.start_notify,
            timeout,
            count,
            || self.starts(),
            "provider starts",
        )
        .await
    }

    /// Releases exactly `count` pending starts in unique-arrival order.
    pub fn release_starts(&self, count: usize) {
        if count == 0 {
            return;
        }
        let pending = {
            let observations = lock_unpoisoned(&self.state.observations);
            observations
                .starts
                .iter()
                .filter_map(|start| {
                    observations
                        .start_gates
                        .get(&start.context.external_job_uid)
                })
                .filter(|gate| !gate.is_released())
                .take(count)
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(
            pending.len(),
            count,
            "release_starts({count}) requires {count} pending fixture starts"
        );
        for gate in pending {
            gate.release();
        }
    }

    /// Returns unique provider starts in deterministic arrival order.
    #[must_use]
    pub fn starts(&self) -> Vec<FixtureExternalJobStart> {
        lock_unpoisoned(&self.state.observations).starts.clone()
    }

    /// Waits for at least `count` provider start-recovery requests.
    pub async fn wait_for_recoveries(
        &self,
        count: usize,
        timeout: Duration,
    ) -> Result<Vec<FixtureExternalJobRecovery>> {
        wait_for_observations(
            &self.state.recovery_notify,
            timeout,
            count,
            || self.recoveries(),
            "provider start recoveries",
        )
        .await
    }

    /// Returns all provider start-recovery lookups in arrival order.
    #[must_use]
    pub fn recoveries(&self) -> Vec<FixtureExternalJobRecovery> {
        lock_unpoisoned(&self.state.observations).recoveries.clone()
    }

    /// Waits for at least `count` post-bind, pre-release barriers.
    pub async fn wait_for_after_bind(
        &self,
        count: usize,
        timeout: Duration,
    ) -> Result<Vec<FixtureExternalJobAfterBind>> {
        wait_for_observations(
            &self.state.after_bind_notify,
            timeout,
            count,
            || self.after_bind(),
            "post-bind barriers",
        )
        .await
    }

    /// Returns unique post-bind barriers in arrival order.
    #[must_use]
    pub fn after_bind(&self) -> Vec<FixtureExternalJobAfterBind> {
        lock_unpoisoned(&self.state.observations).after_bind.clone()
    }

    /// Releases exactly `count` pending post-bind barriers in arrival order.
    pub fn release_after_bind(&self, count: usize) {
        if count == 0 {
            return;
        }
        let pending = {
            let observations = lock_unpoisoned(&self.state.observations);
            observations
                .after_bind
                .iter()
                .filter_map(|barrier| {
                    observations
                        .after_bind_gates
                        .get(&barrier.context.external_job_uid)
                })
                .filter(|gate| !gate.is_released())
                .take(count)
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(
            pending.len(),
            count,
            "release_after_bind({count}) requires {count} pending fixture barriers"
        );
        for gate in pending {
            gate.release();
        }
    }

    /// Waits for at least `count` sparse reconciliation observations.
    pub async fn wait_for_reconciliations(
        &self,
        count: usize,
        timeout: Duration,
    ) -> Result<Vec<FixtureExternalJobReconciliation>> {
        wait_for_observations(
            &self.state.reconcile_notify,
            timeout,
            count,
            || self.reconciliations(),
            "provider reconciliations",
        )
        .await
    }

    /// Returns all sparse reconciliation observations in arrival order.
    #[must_use]
    pub fn reconciliations(&self) -> Vec<FixtureExternalJobReconciliation> {
        lock_unpoisoned(&self.state.observations)
            .reconciliations
            .clone()
    }

    /// Queues exact outcomes consumed by subsequent sparse reconciliations.
    pub fn queue_reconcile_outcomes(
        &self,
        outcomes: impl IntoIterator<Item = AsyncToolJobCallbackOutcome>,
    ) {
        lock_unpoisoned(&self.state.observations)
            .reconcile_outcomes
            .extend(outcomes);
    }

    /// Waits for at least `count` provider cancellation requests.
    pub async fn wait_for_cancellations(
        &self,
        count: usize,
        timeout: Duration,
    ) -> Result<Vec<FixtureExternalJobCancellation>> {
        wait_for_observations(
            &self.state.cancel_notify,
            timeout,
            count,
            || self.cancellations(),
            "provider cancellations",
        )
        .await
    }

    /// Returns all provider cancellation observations in arrival order.
    #[must_use]
    pub fn cancellations(&self) -> Vec<FixtureExternalJobCancellation> {
        lock_unpoisoned(&self.state.observations)
            .cancellations
            .clone()
    }

    /// Queues exact outcomes consumed by subsequent cancellation requests.
    pub fn queue_cancel_outcomes(
        &self,
        outcomes: impl IntoIterator<Item = AsyncToolJobCancelOutcome>,
    ) {
        lock_unpoisoned(&self.state.observations)
            .cancel_outcomes
            .extend(outcomes);
    }

    /// Creates the raw callback envelope parsed by the fixture adapter.
    #[must_use]
    pub fn callback_body(
        &self,
        provider_job_id: impl Into<String>,
        provider_event_id: impl Into<String>,
        outcome: AsyncToolJobCallbackOutcome,
    ) -> serde_json::Value {
        serde_json::json!({
            "provider_job_id": provider_job_id.into(),
            "provider_event_id": provider_event_id.into(),
            "outcome": outcome,
        })
    }
}

/// Running deterministic asynchronous-provider fixture.
pub struct FixtureExternalJobRuntime {
    controller: FixtureExternalJobController,
    endpoint: String,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl FixtureExternalJobRuntime {
    /// Starts the fixture server on one ephemeral loopback port.
    pub async fn start() -> Result<Self> {
        let state = Arc::new(FixtureExternalJobState::default());
        let controller = FixtureExternalJobController {
            state: Arc::clone(&state),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("bind external-job fixture listener")?;
        let address = listener
            .local_addr()
            .context("read external-job fixture listener address")?;
        let endpoint = format!("http://{address}");
        let router = Router::new()
            .route("/start", post(start))
            .route("/recover_start", post(recover_start))
            .route("/after_bind", post(after_bind))
            .route("/cancel", post(cancel))
            .route("/reconcile", post(reconcile))
            .with_state(state);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let server = axum::serve(listener, router).with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            });
            if let Err(error) = server.await {
                tracing::warn!(%error, "external-job fixture server stopped unexpectedly");
            }
        });
        Ok(Self {
            controller,
            endpoint,
            shutdown: Some(shutdown_tx),
            task: Some(task),
        })
    }

    /// Returns the base URL configured on the provider-override adapter.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Returns the observation and release controller.
    #[must_use]
    pub fn controller(&self) -> &FixtureExternalJobController {
        &self.controller
    }

    /// Stops the listener and aborts its accept task.
    pub fn stop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for FixtureExternalJobRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Default)]
struct FixtureExternalJobState {
    observations: StdMutex<FixtureExternalJobObservations>,
    start_notify: Notify,
    recovery_notify: Notify,
    after_bind_notify: Notify,
    reconcile_notify: Notify,
    cancel_notify: Notify,
}

#[derive(Default)]
struct FixtureExternalJobObservations {
    starts: Vec<FixtureExternalJobStart>,
    start_gates: HashMap<Uuid, Arc<ReleaseGate>>,
    jobs: HashMap<Uuid, AsyncToolJob>,
    recoveries: Vec<FixtureExternalJobRecovery>,
    after_bind: Vec<FixtureExternalJobAfterBind>,
    after_bind_gates: HashMap<Uuid, Arc<ReleaseGate>>,
    reconciliations: Vec<FixtureExternalJobReconciliation>,
    reconcile_outcomes: VecDeque<AsyncToolJobCallbackOutcome>,
    cancellations: Vec<FixtureExternalJobCancellation>,
    cancel_outcomes: VecDeque<AsyncToolJobCancelOutcome>,
}

#[derive(Default)]
struct ReleaseGate {
    released: StdMutex<bool>,
    notify: Notify,
}

impl ReleaseGate {
    fn is_released(&self) -> bool {
        *lock_unpoisoned(&self.released)
    }

    fn release(&self) {
        let mut released = lock_unpoisoned(&self.released);
        if !*released {
            *released = true;
            drop(released);
            self.notify.notify_waiters();
        }
    }

    async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_released() {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureStartRequest {
    context: ExternalJobStartContext,
    call: serde_json::Value,
}

#[derive(Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
enum FixtureStartOutcome {
    ExternalJob(AsyncToolJob),
}

#[derive(Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
enum FixtureStartRecovery {
    Started(AsyncToolJob),
}

async fn start(
    State(state): State<Arc<FixtureExternalJobState>>,
    Json(request): Json<FixtureStartRequest>,
) -> Result<Json<FixtureStartOutcome>, axum::http::StatusCode> {
    if request.context.provider != FIXTURE_EXTERNAL_JOB_PROVIDER {
        return Err(axum::http::StatusCode::BAD_REQUEST);
    }
    let (job, gate, is_new) = {
        let mut observations = lock_unpoisoned(&state.observations);
        if let Some(job) = observations
            .jobs
            .get(&request.context.external_job_uid)
            .cloned()
        {
            let gate = observations
                .start_gates
                .get(&request.context.external_job_uid)
                .cloned()
                .ok_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
            (job, gate, false)
        } else {
            let job = fixture_job(&request.context);
            let gate = Arc::new(ReleaseGate::default());
            let start = FixtureExternalJobStart {
                context: request.context.clone(),
                call: request.call,
                provider_job_id: job.provider_job_id.clone(),
                arrival_order: observations.starts.len() as u64 + 1,
            };
            observations.starts.push(start);
            observations
                .start_gates
                .insert(request.context.external_job_uid, Arc::clone(&gate));
            observations
                .jobs
                .insert(request.context.external_job_uid, job.clone());
            (job, gate, true)
        }
    };
    if is_new {
        state.start_notify.notify_waiters();
    }
    gate.wait().await;
    Ok(Json(FixtureStartOutcome::ExternalJob(job)))
}

async fn recover_start(
    State(state): State<Arc<FixtureExternalJobState>>,
    Json(context): Json<ExternalJobStartContext>,
) -> Result<Json<FixtureStartRecovery>, axum::http::StatusCode> {
    let job = {
        let mut observations = lock_unpoisoned(&state.observations);
        let job = observations
            .jobs
            .get(&context.external_job_uid)
            .cloned()
            .ok_or(axum::http::StatusCode::NOT_FOUND)?;
        let arrival_order = observations.recoveries.len() as u64 + 1;
        observations.recoveries.push(FixtureExternalJobRecovery {
            context,
            arrival_order,
        });
        job
    };
    state.recovery_notify.notify_waiters();
    Ok(Json(FixtureStartRecovery::Started(job)))
}

async fn after_bind(
    State(state): State<Arc<FixtureExternalJobState>>,
    Json(context): Json<ExternalJobStartContext>,
) -> Result<Json<()>, axum::http::StatusCode> {
    let (gate, is_new) = {
        let mut observations = lock_unpoisoned(&state.observations);
        if !observations.jobs.contains_key(&context.external_job_uid) {
            return Err(axum::http::StatusCode::NOT_FOUND);
        }
        if let Some(gate) = observations
            .after_bind_gates
            .get(&context.external_job_uid)
            .cloned()
        {
            (gate, false)
        } else {
            let gate = Arc::new(ReleaseGate::default());
            let arrival_order = observations.after_bind.len() as u64 + 1;
            observations.after_bind.push(FixtureExternalJobAfterBind {
                context: context.clone(),
                arrival_order,
            });
            observations
                .after_bind_gates
                .insert(context.external_job_uid, Arc::clone(&gate));
            (gate, true)
        }
    };
    if is_new {
        state.after_bind_notify.notify_waiters();
    }
    gate.wait().await;
    Ok(Json(()))
}

async fn cancel(
    State(state): State<Arc<FixtureExternalJobState>>,
    Json(request): Json<ExecutionExternalJobCancelRequest>,
) -> Json<AsyncToolJobCancelOutcome> {
    let outcome = {
        let mut observations = lock_unpoisoned(&state.observations);
        let arrival_order = observations.cancellations.len() as u64 + 1;
        observations
            .cancellations
            .push(FixtureExternalJobCancellation {
                request,
                arrival_order,
            });
        observations
            .cancel_outcomes
            .pop_front()
            .unwrap_or(AsyncToolJobCancelOutcome::Unsupported)
    };
    state.cancel_notify.notify_waiters();
    Json(outcome)
}

async fn reconcile(
    State(state): State<Arc<FixtureExternalJobState>>,
    Json(request): Json<ExecutionExternalJobReconcileRequest>,
) -> Result<Json<AsyncToolJobCallbackOutcome>, axum::http::StatusCode> {
    let outcome = {
        let mut observations = lock_unpoisoned(&state.observations);
        let arrival_order = observations.reconciliations.len() as u64 + 1;
        observations
            .reconciliations
            .push(FixtureExternalJobReconciliation {
                request,
                arrival_order,
            });
        observations.reconcile_outcomes.pop_front()
    };
    state.reconcile_notify.notify_waiters();
    outcome
        .map(Json)
        .ok_or(axum::http::StatusCode::SERVICE_UNAVAILABLE)
}

fn fixture_job(context: &ExternalJobStartContext) -> AsyncToolJob {
    AsyncToolJob {
        provider: context.provider.clone(),
        provider_job_id: format!("fixture-job-{}", context.external_job_uid),
        idempotency_key: context.idempotency_key.clone(),
        callback_auth_reference: FIXTURE_EXTERNAL_JOB_CALLBACK_TOKEN.to_string(),
        progress_phase: "queued".to_string(),
        cancel_supported: true,
        next_reconcile_at: Utc::now() + chrono::Duration::seconds(5),
    }
}

async fn wait_for_observations<T>(
    notify: &Notify,
    timeout: Duration,
    count: usize,
    snapshot: impl Fn() -> Vec<T>,
    label: &str,
) -> Result<Vec<T>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let notified = notify.notified();
        let observations = snapshot();
        if observations.len() >= count {
            return Ok(observations);
        }
        tokio::time::timeout_at(deadline, notified)
            .await
            .with_context(|| {
                format!(
                    "external-job fixture observed {} of {count} {label} within {timeout:?}",
                    observations.len()
                )
            })?;
    }
}

fn lock_unpoisoned<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pins: provider work is committed under the reserved identity before the start response is
    // released, and crash recovery resolves the same provider job without a second start.
    #[tokio::test]
    async fn external_job_fixture_recovers_committed_blocked_start_without_replay_offline()
    -> Result<()> {
        let mut runtime = FixtureExternalJobRuntime::start().await?;
        let controller = runtime.controller().clone();
        let endpoint = runtime.endpoint().to_string();
        let context = ExternalJobStartContext {
            external_job_uid: Uuid::now_v7(),
            provider: FIXTURE_EXTERNAL_JOB_PROVIDER.to_string(),
            idempotency_key: "fixture-start-key".to_string(),
        };
        let client = reqwest::Client::new();
        let start_context = context.clone();
        let start = tokio::spawn(async move {
            client
                .post(format!("{endpoint}/start"))
                .json(&serde_json::json!({
                    "context": start_context,
                    "call": {"tool_call_id": "opaque-to-provider-fixture"},
                }))
                .send()
                .await?
                .error_for_status()?
                .json::<serde_json::Value>()
                .await
        });

        let starts = controller
            .wait_for_starts(1, Duration::from_secs(2))
            .await?;
        assert_eq!(starts[0].context, context);
        assert_eq!(starts[0].arrival_order, 1);
        assert_eq!(controller.starts().len(), 1);

        let recovery = reqwest::Client::new()
            .post(format!("{}/recover_start", runtime.endpoint()))
            .json(&context)
            .send()
            .await?
            .error_for_status()?
            .json::<serde_json::Value>()
            .await?;
        assert_eq!(recovery["outcome"], "started");
        assert_eq!(
            recovery["provider_job_id"],
            starts[0].provider_job_id.as_str()
        );
        assert_eq!(controller.recoveries().len(), 1);
        assert_eq!(controller.starts().len(), 1);

        controller.release_starts(1);
        let started = start.await.context("join blocked fixture start")??;
        assert_eq!(started["outcome"], "external_job");
        assert_eq!(
            started["provider_job_id"],
            starts[0].provider_job_id.as_str()
        );
        assert_eq!(started["idempotency_key"], context.idempotency_key);
        let after_bind_endpoint = runtime.endpoint().to_string();
        let after_bind_context = context.clone();
        let after_bind = tokio::spawn(async move {
            reqwest::Client::new()
                .post(format!("{after_bind_endpoint}/after_bind"))
                .json(&after_bind_context)
                .send()
                .await?
                .error_for_status()?
                .json::<serde_json::Value>()
                .await
        });
        let barriers = controller
            .wait_for_after_bind(1, Duration::from_secs(2))
            .await?;
        assert_eq!(barriers[0].context, context);
        controller.release_after_bind(1);
        assert_eq!(
            after_bind.await.context("join post-bind barrier")??,
            serde_json::Value::Null
        );
        runtime.stop();
        Ok(())
    }

    // Pins: sparse reconcile and cancellation scripts are consumed once in request order, and
    // the controller observes the exact generation-fenced request sent by the production adapter.
    #[tokio::test]
    async fn external_job_fixture_routes_generation_fenced_provider_operations_offline()
    -> Result<()> {
        let mut runtime = FixtureExternalJobRuntime::start().await?;
        let controller = runtime.controller().clone();
        let tenant_id = TenantId::from(Uuid::now_v7());
        let external_job_uid = Uuid::now_v7();
        let reconcile = ExecutionExternalJobReconcileRequest {
            tenant_id,
            external_job_uid,
            trigger_uid: Uuid::now_v7(),
            job_generation: 3,
            provider: FIXTURE_EXTERNAL_JOB_PROVIDER.to_string(),
            provider_job_id: "fixture-provider-job".to_string(),
            idempotency_key: "fixture-provider-key".to_string(),
        };
        let next_reconcile_at = Utc::now() + chrono::Duration::minutes(1);
        let progress = AsyncToolJobCallbackOutcome::Progress {
            progress_phase: "working".to_string(),
            next_reconcile_at,
        };
        controller.queue_reconcile_outcomes([progress.clone()]);
        let response = reqwest::Client::new()
            .post(format!("{}/reconcile", runtime.endpoint()))
            .json(&reconcile)
            .send()
            .await?
            .error_for_status()?
            .json::<AsyncToolJobCallbackOutcome>()
            .await?;
        assert_eq!(response, progress);
        let reconciliations = controller
            .wait_for_reconciliations(1, Duration::from_secs(2))
            .await?;
        assert_eq!(reconciliations[0].request, reconcile);

        let cancel = ExecutionExternalJobCancelRequest {
            tenant_id,
            external_job_uid,
            job_generation: 3,
            provider: FIXTURE_EXTERNAL_JOB_PROVIDER.to_string(),
            provider_job_id: "fixture-provider-job".to_string(),
            idempotency_key: "fixture-provider-key".to_string(),
        };
        let accepted = AsyncToolJobCancelOutcome::Accepted {
            next_reconcile_at,
            progress_phase: "cancelling".to_string(),
        };
        controller.queue_cancel_outcomes([accepted.clone()]);
        let response = reqwest::Client::new()
            .post(format!("{}/cancel", runtime.endpoint()))
            .json(&cancel)
            .send()
            .await?
            .error_for_status()?
            .json::<AsyncToolJobCancelOutcome>()
            .await?;
        assert_eq!(response, accepted);
        let cancellations = controller
            .wait_for_cancellations(1, Duration::from_secs(2))
            .await?;
        assert_eq!(cancellations[0].request, cancel);
        runtime.stop();
        Ok(())
    }
}

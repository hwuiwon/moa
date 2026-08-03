//! Restate workflow that admits one live behavior experiment into production paths.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::Utc;
use moa_artifacts::document::{ArtifactDefinition, ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, StoredArtifactRevision};
use moa_core::traits::Identity;
use moa_core::{types::action_policy::ActionRuleScope, types::identifiers::TenantId};
use moa_experiments::model::{
    ExperimentRunRecord, ExperimentRunStatus, ExperimentTrialRecord, ExperimentTrialStatus,
    NewExperimentTrial,
};
use moa_experiments::plan::PlanTrialPager;
use moa_experiments::store::ExperimentStore;
use moa_observability::record_experiment_run;
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_wire::experiments::ArtifactReleaseExperimentBinding;
use moa_wire::experiments::{
    AgentRevisionSimulationVariant, ExperimentRunStatusRequest, ExperimentRunStatusResponse,
};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

use crate::workflows::durable_utc_now;
use crate::workflows::errors::{bad_request, handler_error_message, moa_error_to_handler_error};
use crate::workflows::experiment_cancel::has_pending_cancellation;
use crate::workflows::experiment_errors::plan_expansion_error_to_handler_error;
use crate::workflows::experiment_trial_run::{
    ExperimentTrialRunClient, ExperimentTrialRunWorkflowRequest, trial_workflow_key,
};
use moa_core::types::experiments::ExperimentCancelSignal;

mod plan_expansion;
mod status;

use plan_expansion::{plan_revision_uid_from_run, run_experiment_plan};
use status::{run_status_response, status_response};

const K_RUN_UID: &str = "run_uid";
const K_TENANT_ID: &str = "tenant_id";
const PLAN_CHILD_COMPLETION_WAIT_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// Workflow input for one live behavior experiment run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentRunWorkflowRequest {
    /// Tenant already authorized by `Experiments/run`.
    pub tenant_id: TenantId,
    /// Durable experiment run identifier.
    pub run_uid: Uuid,
    /// Identity snapshot used for audit and normal downstream authz checks.
    pub identity: Identity,
    /// Score run identifier associated with this experiment.
    pub score_run_id: Uuid,
    /// Exact agent revision variants used to override plan target variants.
    #[serde(default)]
    pub agent_revision_variants: Vec<AgentRevisionSimulationVariant>,
    /// Internal artifact-release arms executed through this production run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_evaluation: Option<ArtifactReleaseExperimentBinding>,
}

/// Restate workflow surface for one live behavior experiment run.
#[restate_sdk::workflow]
pub trait ExperimentRun {
    /// Drives an accepted experiment run into the matching production path.
    async fn run(
        request: Json<ExperimentRunWorkflowRequest>,
    ) -> Result<Json<ExperimentRunStatusResponse>, HandlerError>;

    /// Reads the current experiment status after service-layer authorization.
    #[shared]
    async fn status(
        request: Json<ExperimentRunStatusRequest>,
    ) -> Result<Json<ExperimentRunStatusResponse>, HandlerError>;

    /// Forwards cancellation to every active trial.
    #[shared]
    async fn request_cancel(signal: Json<ExperimentCancelSignal>) -> Result<(), HandlerError>;
}

/// Concrete live behavior experiment workflow implementation.
pub struct ExperimentRunImpl {
    pool: sqlx::PgPool,
}

impl ExperimentRunImpl {
    /// Creates an experiment workflow with its durable product store.
    #[must_use]
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

impl ExperimentRun for ExperimentRunImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: called only from Experiments/run after tenant operator authz.
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<ExperimentRunWorkflowRequest>,
    ) -> Result<Json<ExperimentRunStatusResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ExperimentRun", "run");
        let request = request.into_inner();
        if request.run_uid.to_string() != ctx.key() {
            return Err(TerminalError::new_with_code(404, "experiment run id mismatch").into());
        }
        let admitted_run =
            load_run_record(&ctx, request.tenant_id, request.run_uid, &self.pool).await?;
        let scope = admitted_run.scope;
        let plan_revision_uid = plan_revision_uid_from_run(&admitted_run).ok_or_else(|| {
            TerminalError::new("experiment run is missing its required plan revision")
        })?;

        ctx.set(K_RUN_UID, Json(request.run_uid));
        ctx.set(K_TENANT_ID, Json(request.tenant_id));
        annotate_run_span(&request);
        if has_pending_cancellation(&ctx, &scope, request.run_uid, &self.pool).await? {
            return run_status_response(
                &ctx,
                ExperimentRunStatusRequest {
                    tenant_id: request.tenant_id,
                    run_uid: request.run_uid,
                },
                &self.pool,
            )
            .await
            .map(Json::from);
        }

        match run_experiment_plan(&ctx, request.clone(), scope, plan_revision_uid, &self.pool).await
        {
            Ok(response) => Ok(Json(response)),
            Err(error) => {
                let message = handler_error_message(&error);
                let failed_at = durable_utc_now(&ctx, "experiment_utc_now").await?;
                if let Err(update_error) = persist_run_status(
                    &ctx,
                    scope,
                    request.run_uid,
                    ExperimentRunStatus::Failed,
                    Some(message),
                    Some(failed_at),
                    &self.pool,
                )
                .await
                {
                    tracing::warn!(
                        error = ?update_error,
                        run_uid = %request.run_uid,
                        "failed to persist experiment workflow failure"
                    );
                }
                Err(error)
            }
        }
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: called only from Experiments/status after tenant operator authz.
    async fn status(
        &self,
        ctx: SharedWorkflowContext<'_>,
        request: Json<ExperimentRunStatusRequest>,
    ) -> Result<Json<ExperimentRunStatusResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ExperimentRun", "status");
        let request = request.into_inner();
        if request.run_uid.to_string() != ctx.key() {
            return Err(TerminalError::new_with_code(404, "experiment run id mismatch").into());
        }
        let pool = self.pool.clone();
        Ok(ctx
            .run(|| async move { status_response(pool, request).await.map(Json::from) })
            .name("experiment_run_status")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, signal))]
    // SAFETY: control-only cancellation forward after Experiments/cancel authz;
    // the child Session, Execution, and ExperimentTrialRun request_cancel
    // handlers enforce their own authorization.
    async fn request_cancel(
        &self,
        ctx: SharedWorkflowContext<'_>,
        signal: Json<ExperimentCancelSignal>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ExperimentRun", "request_cancel");
        let signal = signal.into_inner();
        // The service atomically marks every active trial cancelled before it
        // signals this workflow. Fan out from that persisted post-cancel state
        // so a crash between the database commit and this handler cannot make
        // the child workflows disappear from the cancellation projection.
        fan_out_cancellation_to_cancelled_trials(&ctx, signal, &self.pool).await?;
        Ok(())
    }
}

/// Signals `request_cancel` on every cancelled trial workflow of this run.
///
/// The cancellation transaction has already projected active trials to
/// `Cancelled`, so this must select that persisted state rather than the former
/// `Dispatched`/`Running` states. The signal is idempotent and best-effort.
async fn fan_out_cancellation_to_cancelled_trials(
    ctx: &SharedWorkflowContext<'_>,
    signal: ExperimentCancelSignal,
    pool: &sqlx::PgPool,
) -> Result<(), HandlerError> {
    let Some(run_uid) = ctx
        .get::<Json<Uuid>>(K_RUN_UID)
        .await?
        .map(Json::into_inner)
    else {
        return Ok(());
    };
    let Some(tenant_id) = ctx
        .get::<Json<TenantId>>(K_TENANT_ID)
        .await?
        .map(Json::into_inner)
    else {
        return Ok(());
    };
    for trial_key in load_cancelled_trial_keys(ctx, tenant_id, run_uid, pool).await? {
        crate::restate_identity::replay_safe_request(
            ctx.workflow_client::<ExperimentTrialRunClient>(trial_workflow_key(
                run_uid, &trial_key,
            ))
            .request_cancel(Json::from(signal.clone())),
        )
        .send();
    }
    Ok(())
}

/// Reads the deterministic keys of trials projected cancelled by the service.
async fn load_cancelled_trial_keys(
    ctx: &SharedWorkflowContext<'_>,
    tenant_id: TenantId,
    run_uid: Uuid,
    pool: &sqlx::PgPool,
) -> Result<Vec<String>, HandlerError> {
    let pool = pool.clone();
    Ok(ctx
        .run(|| async move {
            let store = ExperimentStore::new(pool);
            let scope = store
                .load_run_for_workflow(tenant_id, run_uid)
                .await
                .map_err(moa_error_to_handler_error)?
                .ok_or_else(|| run_not_found(run_uid))?
                .scope;
            let trials = store
                .list_trials(&scope, run_uid, None, i64::MAX)
                .await
                .map_err(moa_error_to_handler_error)?;
            let keys = trials
                .into_iter()
                .filter(|trial| trial_status_needs_cancel_forward(trial.status))
                .map(|trial| trial.trial_key)
                .collect::<Vec<_>>();
            Ok::<_, HandlerError>(Json::from(keys))
        })
        .name("experiment_run_cancelled_trial_keys")
        .await?
        .into_inner())
}

const fn trial_status_needs_cancel_forward(status: ExperimentTrialStatus) -> bool {
    matches!(status, ExperimentTrialStatus::Cancelled)
}

async fn persist_run_status(
    ctx: &WorkflowContext<'_>,
    scope: ActionRuleScope,
    run_uid: Uuid,
    status: ExperimentRunStatus,
    error: Option<String>,
    completed_at: Option<chrono::DateTime<Utc>>,
    pool: &sqlx::PgPool,
) -> Result<(), HandlerError> {
    let pool = pool.clone();
    ctx.run(|| async move {
        update_run_status(pool, scope, run_uid, status, error, completed_at).await?;
        Ok::<_, HandlerError>(Json::from(()))
    })
    .name("experiment_update_run_status")
    .await?;
    Ok(())
}

async fn load_run_record(
    ctx: &WorkflowContext<'_>,
    tenant_id: TenantId,
    run_uid: Uuid,
    pool: &sqlx::PgPool,
) -> Result<ExperimentRunRecord, HandlerError> {
    let pool = pool.clone();
    Ok(ctx
        .run(|| async move {
            ExperimentStore::new(pool)
                .load_run_for_workflow(tenant_id, run_uid)
                .await
                .map_err(moa_error_to_handler_error)?
                .ok_or_else(|| run_not_found(run_uid))
                .map(Json::from)
        })
        .name("experiment_load_run_record")
        .await?
        .into_inner())
}

async fn update_run_status(
    pool: sqlx::PgPool,
    scope: ActionRuleScope,
    run_uid: Uuid,
    status: ExperimentRunStatus,
    error: Option<String>,
    completed_at: Option<chrono::DateTime<Utc>>,
) -> Result<ExperimentRunRecord, HandlerError> {
    let run = ExperimentStore::new(pool)
        .update_run_status(&scope, run_uid, status, error, completed_at)
        .await
        .map_err(moa_error_to_handler_error)?
        .ok_or_else(|| run_not_found(run_uid))?;
    record_experiment_run(run.status.as_str(), run.target_kind.as_str());
    Ok(run)
}

fn annotate_run_span(request: &ExperimentRunWorkflowRequest) {
    let span = tracing::Span::current();
    span.set_attribute("moa.experiment.run_uid", request.run_uid.to_string());
    span.set_attribute("moa.experiment.tenant_id", request.tenant_id.to_string());
    span.set_attribute(
        "moa.experiment.run_score_run_id",
        request.score_run_id.to_string(),
    );
}

fn serialized_payload<T>(field: &'static str, value: &T) -> Result<Value, HandlerError>
where
    T: Serialize,
{
    serde_json::to_value(value).map_err(|error| {
        TerminalError::new(format!("serialize experiment {field} failed: {error}")).into()
    })
}

fn run_not_found(run_uid: Uuid) -> HandlerError {
    TerminalError::new_with_code(404, format!("experiment run {run_uid} not found")).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use moa_artifacts::simulation::ExperimentTargetKind;
    use moa_core::types::identifiers::ModelId;
    use moa_experiments::model::ExperimentSimulatorConfig;

    #[test]
    fn aggregate_status_keeps_parent_running_until_partial_failures_are_final() {
        // Pins: failed trial rows are preserved while pending work keeps the parent run active.
        let failed = trial_record("failed", ExperimentTrialStatus::Failed);
        let pending = trial_record("pending", ExperimentTrialStatus::Accepted);
        let dispatched = trial_record("dispatched", ExperimentTrialStatus::Dispatched);
        let completed = trial_record("completed", ExperimentTrialStatus::Completed);

        assert_eq!(
            plan_expansion::aggregate_status_for_trials(
                &[failed.clone(), pending],
                2,
                ExperimentRunStatus::Running
            ),
            ExperimentRunStatus::Running
        );
        assert_eq!(
            plan_expansion::aggregate_status_for_trials(
                &[failed.clone(), dispatched],
                2,
                ExperimentRunStatus::Running
            ),
            ExperimentRunStatus::Running
        );
        assert_eq!(
            plan_expansion::aggregate_status_for_trials(
                &[failed.clone(), completed],
                2,
                ExperimentRunStatus::Running
            ),
            ExperimentRunStatus::Failed
        );
        assert_eq!(
            plan_expansion::aggregate_error_for_trials(&[failed]).as_deref(),
            Some("1 experiment trial(s) failed")
        );
    }

    #[test]
    fn dispatched_trials_occupy_parent_parallelism_slots() {
        // Pins: a child sent by the parent but not yet running still consumes plan parallelism.
        let dispatched = trial_record("dispatched", ExperimentTrialStatus::Dispatched);
        let running = trial_record("running", ExperimentTrialStatus::Running);
        let accepted = trial_record("accepted", ExperimentTrialStatus::Accepted);

        assert_eq!(
            plan_expansion::active_plan_trial_count(&[dispatched, running, accepted]),
            2
        );
        assert!(plan_expansion::trial_status_occupies_dispatch_slot(
            ExperimentTrialStatus::Dispatched
        ));
    }

    #[test]
    fn aggregate_status_maps_cancelled_remaining_work_clearly() {
        // Pins: once no trial is active, cancelled remaining work makes the parent cancelled.
        let completed = trial_record("completed", ExperimentTrialStatus::Completed);
        let cancelled = trial_record("cancelled", ExperimentTrialStatus::Cancelled);

        assert_eq!(
            plan_expansion::aggregate_status_for_trials(
                &[completed, cancelled],
                2,
                ExperimentRunStatus::Running
            ),
            ExperimentRunStatus::Cancelled
        );
    }

    #[test]
    fn cancellation_fanout_uses_post_transaction_trial_status() {
        // Pins: cancel_run_and_active_trials persists Cancelled before the
        // workflow signal, so fan-out cannot search for the old active states.
        assert!(trial_status_needs_cancel_forward(
            ExperimentTrialStatus::Cancelled
        ));
        assert!(!trial_status_needs_cancel_forward(
            ExperimentTrialStatus::Dispatched
        ));
        assert!(!trial_status_needs_cancel_forward(
            ExperimentTrialStatus::Running
        ));
    }

    #[test]
    fn aggregate_status_requires_exact_expected_trial_cardinality() {
        // Pins: plan expansion inserts trials incrementally. Status reads of an
        // empty or terminal prefix cannot settle the run, and excess rows fail
        // closed instead of being silently excluded from the verdict.
        let completed = trial_record("completed", ExperimentTrialStatus::Completed);
        assert_eq!(
            plan_expansion::aggregate_status_for_trials(&[], 2, ExperimentRunStatus::Accepted),
            ExperimentRunStatus::Accepted
        );
        assert_eq!(
            plan_expansion::aggregate_status_for_trials(
                std::slice::from_ref(&completed),
                2,
                ExperimentRunStatus::Running,
            ),
            ExperimentRunStatus::Running
        );
        assert_eq!(
            plan_expansion::aggregate_status_for_trials(
                std::slice::from_ref(&completed),
                2,
                ExperimentRunStatus::Failed,
            ),
            ExperimentRunStatus::Failed,
            "a terminal failure must not be reopened"
        );
        assert_eq!(
            plan_expansion::aggregate_status_for_trials(
                std::slice::from_ref(&completed),
                2,
                ExperimentRunStatus::Completed,
            ),
            ExperimentRunStatus::Failed,
            "completed before exact cardinality is an inconsistent failure"
        );
        assert_eq!(
            plan_expansion::aggregate_status_for_trials(
                &[completed.clone(), completed.clone()],
                2,
                ExperimentRunStatus::Running,
            ),
            ExperimentRunStatus::Completed
        );
        assert_eq!(
            plan_expansion::aggregate_status_for_trials(
                &[completed.clone(), completed.clone(), completed],
                2,
                ExperimentRunStatus::Running,
            ),
            ExperimentRunStatus::Failed
        );
    }

    fn trial_record(trial_key: &str, status: ExperimentTrialStatus) -> ExperimentTrialRecord {
        let now = Utc::now();
        ExperimentTrialRecord {
            scope: ActionRuleScope::Tenant {
                tenant_id: TenantId::new(),
            },
            resource_envelope:
                crate::workflows::experiment_trial_run::resources::fixture_trial_envelope(),
            trial_uid: Uuid::now_v7(),
            run_uid: fixture_uuid(100),
            trial_key: trial_key.to_string(),
            status,
            target_kind: ExperimentTargetKind::AgentLoop,
            variant_key: "baseline".to_string(),
            plan_revision_uid: fixture_uuid(200),
            persona_id: None,
            profile_id: None,
            scenario_id: None,
            data_bundle_ids: Vec::new(),
            artifact_revision_uids: Vec::new(),
            simulator: ExperimentSimulatorConfig {
                policy: crate::workflows::experiment_trial_run::fixture_simulator_policy(
                    "gpt-5.1-mini",
                ),
                max_turns: 4,
                token_budget: None,
            },
            target_model: Some(ModelId::new("gpt-5.1")),
            seed: None,
            session_id: None,
            execution_run_uid: None,
            score_run_id: Uuid::now_v7(),
            final_evidence_hash: None,
            turn_count: 0,
            stop_reason: None,
            error: None,
            trace_id: None,
            started_at: Some(now),
            completed_at: if plan_expansion::run_status_is_terminal(match status {
                ExperimentTrialStatus::Failed => ExperimentRunStatus::Failed,
                ExperimentTrialStatus::Cancelled => ExperimentRunStatus::Cancelled,
                ExperimentTrialStatus::Completed => ExperimentRunStatus::Completed,
                _ => ExperimentRunStatus::Running,
            }) {
                Some(now)
            } else {
                None
            },
            created_at: now,
            updated_at: now,
        }
    }

    fn fixture_uuid(value: u128) -> Uuid {
        Uuid::from_u128(value)
    }
}

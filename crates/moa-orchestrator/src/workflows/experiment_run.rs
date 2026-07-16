//! Restate workflow that admits one live behavior experiment into production paths.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use moa_artifacts::document::{ArtifactDefinition, ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, StoredArtifactRevision};
use moa_artifacts::simulation::ExperimentTargetKind;
use moa_core::traits::{Identity, IdentityType};
use moa_core::wire::experiments::{
    AgentRevisionSimulationVariant, ExperimentRunStatusRequest, ExperimentRunStatusResponse,
};
use moa_core::wire::turn::QueueMessageRequest;
use moa_core::{
    traits::SessionStore, types::action_policy::ActionRuleScope,
    types::agent::AgentSessionSelection, types::channel::Channel, types::contact::SessionActorRef,
    types::identifiers::ModelId, types::identifiers::SessionId, types::identifiers::TenantId,
    types::session::SessionMeta, types::session::SessionStatus,
};
use moa_experiments::model::{
    ExperimentRunRecord, ExperimentRunStatus, ExperimentTarget, ExperimentTrialRecord,
    ExperimentTrialStatus, ExperimentVariant, NewExperimentTrial,
};
use moa_experiments::plan::{ExpandedPlanTrial, expand_plan_trials};
use moa_experiments::store::ExperimentStore;
use moa_observability::record_experiment_run;
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_session::PostgresSessionStore;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

use crate::objects::session::SessionClient;
use crate::restate_identity::with_identity_headers;
use crate::services::session_store::inner::{
    apply_agent_model_policy, create_session_for_identity, resolve_agent_context_for_session,
};
use crate::workflows::durable_utc_now;
use crate::workflows::errors::{bad_request, handler_error_message, moa_error_to_handler_error};
use crate::workflows::experiment_cancel::{
    K_CANCEL_IDENTITY, K_EXECUTION_CONTACT_ID, K_EXECUTION_RUN_UID, forward_child_cancellation,
};
use crate::workflows::experiment_errors::{
    non_retryable_handler_error, plan_expansion_error_to_handler_error,
};
use crate::workflows::experiment_trial_run::{
    ExperimentTrialRunClient, ExperimentTrialRunWorkflowRequest, trial_workflow_key,
};

mod plan_expansion;
mod status;
pub(crate) mod target_execution;

use plan_expansion::run_experiment_plan;
use status::{run_status_response, status_response};
use target_execution::{run_agent_loop_target, run_execution_template_target};

const K_RUN_UID: &str = "run_uid";
const K_TENANT_ID: &str = "tenant_id";
const K_SCORE_RUN_ID: &str = "score_run_id";
const K_STATUS: &str = "status";
const K_SESSION_ID: &str = "session_id";
const PLAN_CHILD_COMPLETION_WAIT_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// Workflow input for one live behavior experiment run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentRunWorkflowRequest {
    /// Tenant already authorized by `Experiments/run`.
    pub tenant_id: TenantId,
    /// Durable experiment run identifier.
    pub run_uid: Uuid,
    /// Serialized experiment target payload.
    pub target: Value,
    /// Serialized experiment variant payload.
    pub variant: Value,
    /// Pinned published experiment_plan revision when this run fans out trials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_revision_uid: Option<Uuid>,
    /// Identity snapshot used for audit and normal downstream authz checks.
    pub identity: Identity,
    /// Score run identifier associated with this experiment.
    pub score_run_id: Uuid,
    /// Exact agent revision variants used to override plan target variants.
    #[serde(default)]
    pub agent_revision_variants: Vec<AgentRevisionSimulationVariant>,
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

    /// Forwards cancellation to the run's own child target and every active trial.
    #[shared]
    async fn request_cancel(reason: Json<String>) -> Result<(), HandlerError>;
}

/// Concrete live behavior experiment workflow implementation.
pub struct ExperimentRunImpl {
    pool: sqlx::PgPool,
    session_store: Arc<PostgresSessionStore>,
}

impl ExperimentRunImpl {
    /// Creates an experiment workflow with its durable product stores.
    #[must_use]
    pub fn new(pool: sqlx::PgPool, session_store: Arc<PostgresSessionStore>) -> Self {
        Self {
            pool,
            session_store,
        }
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

        ctx.set(K_RUN_UID, Json(request.run_uid));
        ctx.set(K_TENANT_ID, Json(request.tenant_id));
        ctx.set(K_SCORE_RUN_ID, Json(request.score_run_id));
        ctx.set(K_STATUS, Json(ExperimentRunStatus::Running));
        ctx.set(K_CANCEL_IDENTITY, Json(request.identity.clone()));
        annotate_run_span(&request, None);

        match run_experiment_target(&ctx, request.clone(), &self.pool, &self.session_store).await {
            Ok(response) => Ok(Json(response)),
            Err(error) => {
                let message = handler_error_message(&error);
                ctx.set(K_STATUS, Json(ExperimentRunStatus::Failed));
                let failed_at = durable_utc_now(&ctx, "experiment_utc_now").await?;
                if let Err(update_error) = persist_run_status(
                    &ctx,
                    request.tenant_id,
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
        let session_store = self.session_store.clone();
        Ok(ctx
            .run(|| async move {
                status_response(pool, session_store, request)
                    .await
                    .map(Json::from)
            })
            .name("experiment_run_status")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, reason))]
    // SAFETY: control-only cancellation forward after Experiments/cancel authz;
    // the child Session, Execution, and ExperimentTrialRun request_cancel
    // handlers enforce their own authorization.
    async fn request_cancel(
        &self,
        ctx: SharedWorkflowContext<'_>,
        reason: Json<String>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ExperimentRun", "request_cancel");
        let reason = reason.into_inner();
        // Single-target runs drive a child Session or Execution directly.
        forward_child_cancellation(&ctx, reason.clone()).await?;
        // Plan runs fan cancellation out to every active trial workflow so their
        // own child targets stop even while the main run loop is blocked waiting
        // on a child completion signal.
        fan_out_cancellation_to_active_trials(&ctx, reason, &self.pool).await?;
        Ok(())
    }
}

/// Signals `request_cancel` on every active trial workflow of this run.
///
/// Loads the run's trials and forwards cancellation to those still occupying a
/// dispatch slot (`Dispatched`/`Running`), i.e. those with a live trial workflow
/// to stop. The signal is idempotent and best-effort.
async fn fan_out_cancellation_to_active_trials(
    ctx: &SharedWorkflowContext<'_>,
    reason: String,
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
    for trial_key in load_active_trial_keys(ctx, tenant_id, run_uid, pool).await? {
        crate::restate_identity::replay_safe_request(
            ctx.workflow_client::<ExperimentTrialRunClient>(trial_workflow_key(
                run_uid, &trial_key,
            ))
            .request_cancel(Json::from(reason.clone())),
        )
        .send();
    }
    Ok(())
}

/// Reads the deterministic keys of trials that currently hold a dispatch slot.
async fn load_active_trial_keys(
    ctx: &SharedWorkflowContext<'_>,
    tenant_id: TenantId,
    run_uid: Uuid,
    pool: &sqlx::PgPool,
) -> Result<Vec<String>, HandlerError> {
    let pool = pool.clone();
    let scope = tenant_scope(tenant_id);
    Ok(ctx
        .run(|| async move {
            let trials = ExperimentStore::new(pool)
                .list_trials(&scope, run_uid, None, i64::MAX)
                .await
                .map_err(moa_error_to_handler_error)?;
            let keys = trials
                .into_iter()
                .filter(|trial| plan_expansion::trial_status_occupies_dispatch_slot(trial.status))
                .map(|trial| trial.trial_key)
                .collect::<Vec<_>>();
            Ok::<_, HandlerError>(Json::from(keys))
        })
        .name("experiment_run_active_trial_keys")
        .await?
        .into_inner())
}

async fn run_experiment_target(
    ctx: &WorkflowContext<'_>,
    request: ExperimentRunWorkflowRequest,
    pool: &sqlx::PgPool,
    session_store: &Arc<PostgresSessionStore>,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    if let Some(plan_revision_uid) = request.plan_revision_uid {
        return run_experiment_plan(ctx, request, plan_revision_uid, pool, session_store).await;
    }

    match parse_payload::<ExperimentTarget>("target", request.target.clone())? {
        ExperimentTarget::AgentLoop {
            prompt,
            session_id,
            agent,
            model,
            attachments,
        } => {
            annotate_run_span(&request, Some(ExperimentTargetKind::AgentLoop));
            run_agent_loop_target(
                ctx,
                request,
                prompt,
                session_id,
                agent,
                model,
                attachments,
                pool,
                session_store,
            )
            .await
        }
        ExperimentTarget::ExecutionTemplate {
            template,
            objective,
            input,
            session_id,
            idempotency_key,
        } => {
            annotate_run_span(&request, Some(ExperimentTargetKind::ExecutionTemplate));
            run_execution_template_target(
                ctx,
                request,
                template,
                objective,
                input,
                session_id,
                idempotency_key,
                pool,
                session_store,
            )
            .await
        }
    }
}

async fn persist_run_status(
    ctx: &WorkflowContext<'_>,
    tenant_id: TenantId,
    run_uid: Uuid,
    status: ExperimentRunStatus,
    error: Option<String>,
    completed_at: Option<chrono::DateTime<Utc>>,
    pool: &sqlx::PgPool,
) -> Result<(), HandlerError> {
    let pool = pool.clone();
    let scope = tenant_scope(tenant_id);
    ctx.run(|| async move {
        update_run_status(pool, scope, run_uid, status, error, completed_at).await?;
        Ok::<_, HandlerError>(Json::from(()))
    })
    .name("experiment_update_run_status")
    .await?;
    Ok(())
}

async fn persist_attached_session(
    ctx: &WorkflowContext<'_>,
    scope: ActionRuleScope,
    run_uid: Uuid,
    session_id: SessionId,
    pool: &sqlx::PgPool,
) -> Result<(), HandlerError> {
    let pool = pool.clone();
    ctx.run(|| async move {
        attach_session(pool, scope, run_uid, session_id).await?;
        Ok::<_, HandlerError>(Json::from(()))
    })
    .name("experiment_attach_session")
    .await?;
    Ok(())
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

async fn attach_session(
    pool: sqlx::PgPool,
    scope: ActionRuleScope,
    run_uid: Uuid,
    session_id: SessionId,
) -> Result<ExperimentRunRecord, HandlerError> {
    ExperimentStore::new(pool)
        .attach_session(&scope, run_uid, session_id)
        .await
        .map_err(moa_error_to_handler_error)?
        .ok_or_else(|| run_not_found(run_uid))
}

async fn attach_execution_run(
    pool: sqlx::PgPool,
    scope: ActionRuleScope,
    run_uid: Uuid,
    execution_run_uid: Uuid,
) -> Result<ExperimentRunRecord, HandlerError> {
    ExperimentStore::new(pool)
        .attach_execution_run(&scope, run_uid, execution_run_uid)
        .await
        .map_err(moa_error_to_handler_error)?
        .ok_or_else(|| run_not_found(run_uid))
}

fn annotate_run_span(
    request: &ExperimentRunWorkflowRequest,
    target_kind: Option<ExperimentTargetKind>,
) {
    let span = tracing::Span::current();
    span.set_attribute("moa.experiment.run_uid", request.run_uid.to_string());
    span.set_attribute("moa.experiment.tenant_id", request.tenant_id.to_string());
    span.set_attribute(
        "moa.experiment.run_score_run_id",
        request.score_run_id.to_string(),
    );
    if let Some(target_kind) = target_kind {
        span.set_attribute("moa.experiment.target_kind", target_kind.as_str());
    }
}

fn tenant_scope(tenant_id: TenantId) -> ActionRuleScope {
    ActionRuleScope::Tenant { tenant_id }
}

fn parse_payload<T>(field: &'static str, value: Value) -> Result<T, HandlerError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(value).map_err(|error| {
        TerminalError::new_with_code(400, format!("invalid experiment {field}: {error}")).into()
    })
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
    use moa_experiments::model::ExperimentSimulatorConfig;
    use serde_json::json;

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
                ExperimentRunStatus::Running
            ),
            ExperimentRunStatus::Running
        );
        assert_eq!(
            plan_expansion::aggregate_status_for_trials(
                &[failed.clone(), dispatched],
                ExperimentRunStatus::Running
            ),
            ExperimentRunStatus::Running
        );
        assert_eq!(
            plan_expansion::aggregate_status_for_trials(
                &[failed.clone(), completed],
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
                ExperimentRunStatus::Running
            ),
            ExperimentRunStatus::Cancelled
        );
    }

    #[test]
    fn aggregate_status_completes_empty_plan_expansion() {
        // Pins: an empty runtime expansion cannot leave the parent polling forever.
        assert_eq!(
            plan_expansion::aggregate_status_for_trials(&[], ExperimentRunStatus::Running),
            ExperimentRunStatus::Completed
        );
        assert_eq!(
            plan_expansion::aggregate_status_for_trials(&[], ExperimentRunStatus::Cancelled),
            ExperimentRunStatus::Cancelled
        );
    }

    fn trial_record(trial_key: &str, status: ExperimentTrialStatus) -> ExperimentTrialRecord {
        let now = Utc::now();
        ExperimentTrialRecord {
            scope: ActionRuleScope::Tenant {
                tenant_id: TenantId::new(),
            },
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
                model: ModelId::new("gpt-5.1-mini"),
                temperature: Some(0.0),
                max_turns: 4,
                token_budget: None,
                metadata: json!({}),
            },
            target_model: Some(ModelId::new("gpt-5.1")),
            seed: None,
            session_id: None,
            execution_run_uid: None,
            score_run_id: Uuid::now_v7(),
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

//! Restate workflow that admits one live behavior experiment into production paths.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::Utc;
use moa_artifacts::document::{ArtifactDefinition, ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, ArtifactRunStatus, StoredArtifactRevision};
use moa_artifacts::simulation::ExperimentTargetKind;
use moa_core::traits::{Identity, IdentityType};
use moa_core::wire::experiments::{
    AgentRevisionSimulationVariant, ExperimentRunStatusRequest, ExperimentRunStatusResponse,
};
use moa_core::wire::turn::QueueMessageRequest;
use moa_core::{
    ActionRuleScope, AgentSessionSelection, Channel, MoaError, ModelId, SessionActorRef, SessionId,
    SessionMeta, SessionStatus, SessionStore, TenantId,
};
use moa_experiments::model::{
    ExperimentRunRecord, ExperimentRunStatus, ExperimentTarget, ExperimentTrialRecord,
    ExperimentTrialStatus, ExperimentVariant, NewExperimentTrial,
};
use moa_experiments::plan::{ExpandedPlanTrial, PlanExpansionError, expand_plan_trials};
use moa_experiments::store::ExperimentStore;
use moa_observability::record_experiment_run;
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_workflows::runtime::{StartWorkflowRun, WorkflowRuntime};
use restate_sdk::context::Request;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::objects::session::SessionClient;
use crate::services::session_store::inner::{
    apply_agent_model_policy, create_session_for_identity, resolve_agent_context_for_session,
};
use crate::workflows::artifact_workflow_execution::{
    ArtifactWorkflowExecutionClient, RunArtifactWorkflowRequest,
};
use crate::workflows::errors::workflow_handler_error;
use crate::workflows::experiment_trial_run::{
    ExperimentTrialRunClient, ExperimentTrialRunWorkflowRequest, trial_workflow_key,
};

mod plan_expansion;
mod status;
mod target_execution;

use plan_expansion::run_experiment_plan;
use status::{status_response, workflow_status_response};
use target_execution::{run_agent_loop_target, run_workflow_target};

const K_RUN_UID: &str = "run_uid";
const K_SCORE_RUN_ID: &str = "score_run_id";
const K_STATUS: &str = "status";
const K_SESSION_ID: &str = "session_id";
const K_WORKFLOW_RUN_UID: &str = "workflow_run_uid";
const PLAN_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(1);
const PLAN_STATUS_POLL_MAX_INTERVAL: Duration = Duration::from_secs(8);
const PLAN_STATUS_MAX_IDLE_POLLS: u32 = 10_800;

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
}

/// Concrete live behavior experiment workflow implementation.
pub struct ExperimentRunImpl;

impl ExperimentRun for ExperimentRunImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: called only from Experiments/run after tenant operator authz.
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<ExperimentRunWorkflowRequest>,
    ) -> Result<Json<ExperimentRunStatusResponse>, HandlerError> {
        annotate_restate_handler_span("ExperimentRun", "run");
        let request = request.into_inner();
        if request.run_uid.to_string() != ctx.key() {
            return Err(TerminalError::new_with_code(404, "experiment run id mismatch").into());
        }

        ctx.set(K_RUN_UID, Json(request.run_uid));
        ctx.set(K_SCORE_RUN_ID, Json(request.score_run_id));
        ctx.set(K_STATUS, Json(ExperimentRunStatus::Running));
        annotate_run_span(&request, None);

        match run_experiment_target(&ctx, request.clone()).await {
            Ok(response) => Ok(Json(response)),
            Err(error) => {
                let message = handler_error_message(&error);
                ctx.set(K_STATUS, Json(ExperimentRunStatus::Failed));
                let failed_at = durable_utc_now(&ctx).await?;
                if let Err(update_error) = persist_run_status(
                    &ctx,
                    request.tenant_id,
                    request.run_uid,
                    ExperimentRunStatus::Failed,
                    Some(message),
                    Some(failed_at),
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
        annotate_restate_handler_span("ExperimentRun", "status");
        let request = request.into_inner();
        if request.run_uid.to_string() != ctx.key() {
            return Err(TerminalError::new_with_code(404, "experiment run id mismatch").into());
        }
        let pool = OrchestratorCtx::current_graph_pool();
        Ok(ctx
            .run(|| async move { status_response(pool, request).await.map(Json::from) })
            .name("experiment_run_status")
            .await?)
    }
}

async fn run_experiment_target(
    ctx: &WorkflowContext<'_>,
    request: ExperimentRunWorkflowRequest,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    if let Some(plan_revision_uid) = request.plan_revision_uid {
        return run_experiment_plan(ctx, request, plan_revision_uid).await;
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
            run_agent_loop_target(ctx, request, prompt, session_id, agent, model, attachments).await
        }
        ExperimentTarget::Workflow {
            workflow_ref,
            input,
            session_id,
            idempotency_key,
        } => {
            annotate_run_span(&request, Some(ExperimentTargetKind::Workflow));
            run_workflow_target(
                ctx,
                request,
                workflow_ref,
                input,
                session_id,
                idempotency_key,
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
) -> Result<(), HandlerError> {
    let pool = OrchestratorCtx::current_graph_pool();
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
) -> Result<(), HandlerError> {
    let pool = OrchestratorCtx::current_graph_pool();
    ctx.run(|| async move {
        attach_session(pool, scope, run_uid, session_id).await?;
        Ok::<_, HandlerError>(Json::from(()))
    })
    .name("experiment_attach_session")
    .await?;
    Ok(())
}

async fn durable_utc_now(ctx: &WorkflowContext<'_>) -> Result<chrono::DateTime<Utc>, HandlerError> {
    Ok(ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(Utc::now())) })
        .name("experiment_utc_now")
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

async fn attach_workflow_run(
    pool: sqlx::PgPool,
    scope: ActionRuleScope,
    run_uid: Uuid,
    workflow_run_uid: Uuid,
) -> Result<ExperimentRunRecord, HandlerError> {
    ExperimentStore::new(pool)
        .attach_workflow_run(&scope, run_uid, workflow_run_uid)
        .await
        .map_err(moa_error_to_handler_error)?
        .ok_or_else(|| run_not_found(run_uid))
}

fn new_session_meta(
    tenant_id: TenantId,
    model: ModelId,
    identity: &Identity,
) -> Result<SessionMeta, HandlerError> {
    let now = Utc::now();
    Ok(SessionMeta {
        id: SessionId::new(),
        tenant_id,
        title: Some("Experiment agent-loop run".to_string()),
        status: SessionStatus::Created,
        channel: Channel::Chat,
        model,
        created_at: now,
        updated_at: now,
        created_by: Some(session_actor_ref(identity)?),
        ..SessionMeta::default()
    })
}

fn session_actor_ref(identity: &Identity) -> Result<SessionActorRef, HandlerError> {
    match identity.identity_type {
        IdentityType::User | IdentityType::Agent => Ok(SessionActorRef::Identity {
            id: identity.acting_on_behalf_of.unwrap_or(identity.id),
        }),
        IdentityType::Service => Err(TerminalError::new_with_code(
            403,
            "service identities cannot create agent-loop experiment sessions",
        )
        .into()),
        IdentityType::Contact => Err(TerminalError::new_with_code(
            403,
            "contact identities cannot create agent-loop experiment sessions",
        )
        .into()),
    }
}

fn with_identity_headers<'a, Req, Res>(
    request: Request<'a, Req, Res>,
    identity: &Identity,
) -> Request<'a, Req, Res> {
    let request = request
        .header(
            "x-moa-identity-type".to_string(),
            identity_type_header(identity.identity_type).to_string(),
        )
        .header("x-moa-identity-id".to_string(), identity.id.to_string())
        .header(
            "x-moa-tenant-id".to_string(),
            identity.tenant_id.to_string(),
        );
    let request = if let Some(api_key_id) = identity.api_key_id {
        request.header("x-moa-api-key-id".to_string(), api_key_id.to_string())
    } else {
        request
    };
    if let Some(user_id) = identity.acting_on_behalf_of {
        request.header("x-moa-acting-on-behalf-of".to_string(), user_id.to_string())
    } else {
        request
    }
}

fn identity_type_header(identity_type: IdentityType) -> &'static str {
    match identity_type {
        IdentityType::User => "user",
        IdentityType::Agent => "agent",
        IdentityType::Service => "service",
        IdentityType::Contact => "contact",
    }
}

fn workflow_runtime(pool: sqlx::PgPool) -> WorkflowRuntime {
    WorkflowRuntime::new(ArtifactRegistry::new(pool))
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

fn bad_request(message: impl Into<String>) -> HandlerError {
    TerminalError::new_with_code(400, message.into()).into()
}

fn run_not_found(run_uid: Uuid) -> HandlerError {
    TerminalError::new_with_code(404, format!("experiment run {run_uid} not found")).into()
}

fn moa_error_to_handler_error(error: MoaError) -> HandlerError {
    if error.is_fatal() {
        return TerminalError::new(error.to_string()).into();
    }

    HandlerError::from(error)
}

fn plan_expansion_error_to_handler_error(error: PlanExpansionError) -> HandlerError {
    bad_request(error.to_string())
}

fn non_retryable_handler_error(error: HandlerError) -> HandlerError {
    TerminalError::new(handler_error_message(&error)).into()
}

fn handler_error_message(error: &HandlerError) -> String {
    let error_ref = <HandlerError as AsRef<dyn std::error::Error + Send + Sync>>::as_ref(error);
    error_ref.to_string()
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

    #[test]
    fn plan_status_poll_interval_backs_off_when_idle() {
        // Pins: the parent plan loop backs off bounded idle scans.
        assert_eq!(
            plan_expansion::plan_status_poll_interval(0),
            Duration::from_secs(1)
        );
        assert_eq!(
            plan_expansion::plan_status_poll_interval(1),
            Duration::from_secs(2)
        );
        assert_eq!(
            plan_expansion::plan_status_poll_interval(2),
            Duration::from_secs(4)
        );
        assert_eq!(
            plan_expansion::plan_status_poll_interval(9),
            Duration::from_secs(8)
        );
    }

    #[test]
    fn plan_idle_poll_counter_resets_on_progress_and_has_a_limit() {
        // Pins: unchanged plan state cannot keep a parent workflow polling forever.
        assert_eq!(plan_expansion::next_plan_idle_polls(9, true), 0);
        assert_eq!(plan_expansion::next_plan_idle_polls(9, false), 10);
        assert_eq!(
            plan_expansion::next_plan_idle_polls(u32::MAX, false),
            u32::MAX
        );
        assert!(!plan_expansion::plan_idle_limit_exceeded(
            PLAN_STATUS_MAX_IDLE_POLLS - 1
        ));
        assert!(plan_expansion::plan_idle_limit_exceeded(
            PLAN_STATUS_MAX_IDLE_POLLS
        ));
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
            workflow_run_uid: None,
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

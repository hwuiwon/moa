//! Restate workflow that executes one behavior-lab simulator trial.

use std::time::Duration;

use chrono::Utc;
use moa_artifacts::document::{ArtifactDefinition, ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, ArtifactRunStatus};
use moa_artifacts::simulation::ExperimentTargetKind;
use moa_core::traits::{Identity, IdentityType};
use moa_core::wire::turn::{QueueMessageRequest, SessionSnapshot};
use moa_core::{
    ActionRuleScope, AgentSessionSelection, Channel, CompletionRequest, ContextMessage, Event,
    EventRange, EventRecord, EventType, MoaError, ModelId, SessionActorRef, SessionId, SessionMeta,
    SessionStatus, TenantId,
};
use moa_experiments::model::{
    ExperimentTarget, ExperimentTrialRecord, ExperimentTrialStatus, ExperimentTrialStopReason,
    ExperimentVariant, NewExperimentTrial,
};
use moa_experiments::plan::{PlanExpansionError, PlanSimulationSelection, select_simulation};
use moa_experiments::store::ExperimentStore;
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_observability::{
    current_trace_id, record_experiment_trial, record_experiment_trial_duration,
    record_simulation_cost_cents, record_simulation_tokens, record_simulation_turn,
};
use moa_workflows::runtime::{StartWorkflowRun, WorkflowRuntime};
use restate_sdk::context::Request;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::objects::session::SessionClient;
use crate::services::llm_gateway::{LLMGatewayImpl, compute_cost_cents};
use crate::services::session_store::inner::{
    apply_agent_model_policy, create_session_for_identity, resolve_agent_context_for_session,
};
use crate::workflows::artifact_workflow_execution::{
    ArtifactWorkflowExecutionClient, RunArtifactWorkflowRequest,
};
use crate::workflows::errors::workflow_handler_error;

mod status;
mod target_execution;
mod trial_simulator;

use status::{
    attach_current_trial_trace, insert_or_load_trial, persist_trial_status,
    persist_trial_status_by_key, status_response_from_record, trial_status_allows_child_start,
    trial_status_response,
};
use target_execution::{run_agent_loop_trial, run_workflow_trial};
use trial_simulator::load_simulator_context;

const K_RUN_UID: &str = "run_uid";
const K_TRIAL_UID: &str = "trial_uid";
const K_TRIAL_KEY: &str = "trial_key";
const K_STATUS: &str = "status";
const K_SESSION_ID: &str = "session_id";
const K_WORKFLOW_RUN_UID: &str = "workflow_run_uid";
const TARGET_WAIT_ATTEMPTS: u32 = 90;
const TARGET_WAIT_INTERVAL: Duration = Duration::from_secs(1);
const SESSION_AUTHZ_PROPAGATION_DELAY: Duration = Duration::from_millis(750);

/// Workflow input for one behavior-lab simulator trial.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentTrialRunWorkflowRequest {
    /// Tenant already authorized by the experiment planner or service.
    pub tenant_id: TenantId,
    /// Trial fields used for idempotent insert-or-load by `(run_uid, trial_key)`.
    pub trial: NewExperimentTrial,
    /// Serialized experiment target payload selected for this trial.
    pub target: Value,
    /// Serialized experiment variant payload selected for this trial.
    pub variant: Value,
    /// Identity snapshot used for normal downstream authz checks.
    pub identity: Identity,
}

/// Request payload for reading one trial workflow status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentTrialRunStatusRequest {
    /// Tenant used for trial-result filtering.
    pub tenant_id: TenantId,
    /// Stable trial identifier.
    pub trial_uid: Uuid,
}

/// Response payload for one trial workflow status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentTrialRunStatusResponse {
    /// Tenant that owns the trial.
    pub tenant_id: TenantId,
    /// Experiment run that owns the trial.
    pub run_uid: Uuid,
    /// Stable trial identifier.
    pub trial_uid: Uuid,
    /// Deterministic key unique inside the owning run.
    pub trial_key: String,
    /// Current trial lifecycle status.
    pub status: String,
    /// Execution shape targeted by this trial.
    pub target_kind: String,
    /// Durable stop reason when the trial has stopped.
    pub stop_reason: Option<String>,
    /// Number of simulator user turns submitted to the target.
    pub turn_count: i32,
    /// Linked target session.
    pub session_id: Option<SessionId>,
    /// Linked artifact workflow run.
    pub workflow_run_uid: Option<Uuid>,
    /// Score run identifier used for trial-level scores.
    pub score_run_id: Uuid,
    /// Terminal error for failed trials.
    pub error: Option<String>,
    /// Full trial record payload for service versions that can expose it.
    #[serde(default)]
    pub trial: Value,
}

/// Restate workflow surface for one behavior-lab simulator trial.
#[restate_sdk::workflow]
pub trait ExperimentTrialRun {
    /// Executes one simulator-target trial through the configured production path.
    async fn run(
        request: Json<ExperimentTrialRunWorkflowRequest>,
    ) -> Result<Json<ExperimentTrialRunStatusResponse>, HandlerError>;

    /// Reads the current persisted trial status.
    #[shared]
    async fn status(
        request: Json<ExperimentTrialRunStatusRequest>,
    ) -> Result<Json<ExperimentTrialRunStatusResponse>, HandlerError>;
}

/// Concrete behavior-lab trial workflow implementation.
pub struct ExperimentTrialRunImpl;

impl ExperimentTrialRun for ExperimentTrialRunImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: dispatched only from authorized experiment execution paths.
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<ExperimentTrialRunWorkflowRequest>,
    ) -> Result<Json<ExperimentTrialRunStatusResponse>, HandlerError> {
        annotate_restate_handler_span("ExperimentTrialRun", "run");
        let request = request.into_inner();
        let expected_key = trial_workflow_key(request.trial.run_uid, &request.trial.trial_key);
        if expected_key != ctx.key() {
            return Err(TerminalError::new_with_code(404, "experiment trial key mismatch").into());
        }

        ctx.set(K_RUN_UID, Json(request.trial.run_uid));
        ctx.set(K_TRIAL_KEY, Json(request.trial.trial_key.clone()));
        annotate_trial_span(&request.trial, None);

        match run_trial(&ctx, request.clone()).await {
            Ok(response) => Ok(Json(response)),
            Err(error) => {
                let message = handler_error_message(&error);
                ctx.set(K_STATUS, Json(ExperimentTrialStatus::Failed));
                if let Err(update_error) = persist_trial_status_by_key(
                    &ctx,
                    request.tenant_id,
                    request.trial.run_uid,
                    request.trial.trial_key.clone(),
                    ExperimentTrialStatus::Failed,
                    Some(ExperimentTrialStopReason::Error),
                    Some(message),
                )
                .await
                {
                    tracing::warn!(
                        error = ?update_error,
                        run_uid = %request.trial.run_uid,
                        trial_key = %request.trial.trial_key,
                        "failed to persist experiment trial workflow failure"
                    );
                }
                Err(error)
            }
        }
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: called only after experiment status authorization.
    async fn status(
        &self,
        ctx: SharedWorkflowContext<'_>,
        request: Json<ExperimentTrialRunStatusRequest>,
    ) -> Result<Json<ExperimentTrialRunStatusResponse>, HandlerError> {
        annotate_restate_handler_span("ExperimentTrialRun", "status");
        let request = request.into_inner();
        let pool = OrchestratorCtx::current_graph_pool();
        Ok(ctx
            .run(|| async move { trial_status_response(pool, request).await.map(Json::from) })
            .name("experiment_trial_status")
            .await?)
    }
}

/// Returns the stable Restate workflow key for a trial retry.
#[must_use]
pub fn trial_workflow_key(run_uid: Uuid, trial_key: &str) -> String {
    format!("{run_uid}:{trial_key}")
}

async fn run_trial(
    ctx: &WorkflowContext<'_>,
    request: ExperimentTrialRunWorkflowRequest,
) -> Result<ExperimentTrialRunStatusResponse, HandlerError> {
    let trial = insert_or_load_trial(ctx, request.tenant_id, request.trial.clone()).await?;
    ctx.set(K_TRIAL_UID, Json(trial.trial_uid));
    ctx.set(K_STATUS, Json(trial.status));
    annotate_trial_record_span(&trial);
    attach_current_trial_trace(ctx, request.tenant_id, trial.trial_uid).await?;
    if !trial_status_allows_child_start(trial.status) {
        return status_response_from_record(request.tenant_id, trial);
    }

    let trial = persist_trial_status(
        ctx,
        request.tenant_id,
        trial.trial_uid,
        ExperimentTrialStatus::Running,
        None,
        None,
    )
    .await?;
    ctx.set(K_STATUS, Json(trial.status));

    let simulator_context = load_simulator_context(ctx, request.tenant_id, trial.clone()).await?;
    match trial.target_kind {
        ExperimentTargetKind::AgentLoop => {
            run_agent_loop_trial(ctx, request, trial, simulator_context).await
        }
        ExperimentTargetKind::Workflow => run_workflow_trial(ctx, request, trial).await,
    }
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
        title: Some("Experiment trial agent-loop run".to_string()),
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
            "service identities cannot create agent-loop experiment trial sessions",
        )
        .into()),
        IdentityType::Contact => Err(TerminalError::new_with_code(
            403,
            "contact identities cannot create agent-loop experiment trial sessions",
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

fn annotate_trial_span(trial: &NewExperimentTrial, trial_uid: Option<Uuid>) {
    let span = tracing::Span::current();
    span.set_attribute("moa.experiment.run_uid", trial.run_uid.to_string());
    span.set_attribute("moa.experiment.trial_key", trial.trial_key.clone());
    span.set_attribute("moa.experiment.target_kind", trial.target_kind.as_str());
    span.set_attribute(
        "moa.experiment.score_run_id",
        trial.score_run_id.to_string(),
    );
    if let Some(trial_uid) = trial_uid {
        span.set_attribute("moa.experiment.trial_uid", trial_uid.to_string());
    }
}

fn annotate_trial_record_span(trial: &ExperimentTrialRecord) {
    let span = tracing::Span::current();
    span.set_attribute("moa.experiment.run_uid", trial.run_uid.to_string());
    span.set_attribute("moa.experiment.trial_uid", trial.trial_uid.to_string());
    span.set_attribute("moa.experiment.trial_key", trial.trial_key.clone());
    span.set_attribute("moa.experiment.target_kind", trial.target_kind.as_str());
    span.set_attribute(
        "moa.experiment.score_run_id",
        trial.score_run_id.to_string(),
    );
}

fn tenant_scope(tenant_id: TenantId) -> ActionRuleScope {
    ActionRuleScope::Tenant { tenant_id }
}

fn parse_payload<T>(field: &'static str, value: Value) -> Result<T, HandlerError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(value).map_err(|error| {
        TerminalError::new_with_code(400, format!("invalid experiment trial {field}: {error}"))
            .into()
    })
}

fn artifact_revision_not_found(revision_uid: Uuid) -> HandlerError {
    TerminalError::new_with_code(404, format!("artifact revision {revision_uid} not found")).into()
}

fn trial_not_found(trial_uid: Uuid) -> HandlerError {
    TerminalError::new_with_code(404, format!("experiment trial {trial_uid} not found")).into()
}

fn bad_request(message: impl Into<String>) -> HandlerError {
    TerminalError::new_with_code(400, message.into()).into()
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

    #[test]
    fn simulator_done_accepts_empty_and_done_markers() {
        // Pins: an empty or explicit simulator stop does not enter the target transcript.
        assert!(trial_simulator::simulator_done(""));
        assert!(trial_simulator::simulator_done(" DONE "));
        assert!(trial_simulator::simulator_done("[done]"));
        assert!(!trial_simulator::simulator_done(
            "I still need help with this order."
        ));
    }

    #[test]
    fn effective_max_turns_uses_scenario_cap_when_lower() {
        // Pins: scenario max_turns bounds the simulator loop without allowing zero-turn trials.
        assert_eq!(trial_simulator::effective_max_turns(5, 2), 2);
        assert_eq!(trial_simulator::effective_max_turns(0, 2), 1);
        assert_eq!(trial_simulator::effective_max_turns(3, 0), 3);
    }

    #[test]
    fn trial_workflow_key_is_stable_for_retries() {
        // Pins: Restate workflow retry identity is based on the run UID and deterministic trial key.
        let run_uid = Uuid::nil();
        assert_eq!(
            trial_workflow_key(run_uid, "persona-a/scenario-b/variant-c/0"),
            "00000000-0000-0000-0000-000000000000:persona-a/scenario-b/variant-c/0"
        );
    }
}

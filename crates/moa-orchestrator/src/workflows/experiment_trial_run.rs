//! Restate workflow that executes one behavior-lab simulator trial.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use moa_artifacts::document::{ArtifactDefinition, ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::ArtifactRegistry;
use moa_artifacts::simulation::ExperimentTargetKind;
use moa_config::MoaConfig;
use moa_core::traits::{Identity, IdentityType};
use moa_core::{
    events::Event, events::EventType, traits::SessionStore, types::action_policy::ActionRuleScope,
    types::action_policy::CallOrigin, types::agent::AgentSessionSelection, types::channel::Channel,
    types::completion::CompletionRequest, types::contact::SessionActorRef,
    types::context::ContextMessage, types::events_stream::EventRange,
    types::events_stream::EventRecord, types::identifiers::ModelId, types::identifiers::SessionId,
    types::identifiers::TenantId, types::session::SessionMeta, types::session::SessionStatus,
};
use moa_experiments::model::{
    ExperimentTarget, ExperimentTrialRecord, ExperimentTrialStatus, ExperimentTrialStopReason,
    ExperimentVariant, NewExperimentTrial,
};
use moa_experiments::plan::{PlanSimulationSelection, select_simulation};
use moa_experiments::store::ExperimentStore;
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_observability::{
    current_trace_id, record_experiment_trial, record_simulation_cost_cents,
    record_simulation_tokens, record_simulation_turn,
};
use moa_providers::ProviderRegistry;
use moa_session::PostgresSessionStore;
use moa_wire::turn::{QueueMessageRequest, TurnOutcome, TurnOutcomeKind};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

use crate::lineage::ScoreLineageHandle;
use crate::objects::session::SessionClient;
use crate::restate_identity::with_identity_headers;
use crate::services::llm_gateway::{LLMGatewayImpl, compute_cost_cents};
use crate::services::session_store::inner::{
    apply_agent_model_policy, create_session_for_identity, resolve_agent_context_for_session,
};
use crate::workflows::errors::{bad_request, handler_error_message, moa_error_to_handler_error};
use crate::workflows::experiment_cancel::{
    K_EXECUTION_CONTACT_ID, K_EXECUTION_RUN_UID, forward_child_cancellation,
    forward_pending_child_cancellation, has_pending_cancellation,
};
use crate::workflows::experiment_errors::{
    non_retryable_handler_error, plan_expansion_error_to_handler_error,
};
use moa_core::types::experiments::ExperimentCancelSignal;

mod finalize;
mod status;
mod target_execution;
mod trial_simulator;

use finalize::{TrialFinalization, finalize_trial};
use status::{
    attach_current_trial_trace, insert_or_load_trial, persist_trial_status,
    persist_trial_status_by_key, status_response_from_record, trial_status_allows_child_start,
    trial_status_response,
};
use target_execution::{TrialTargetOutcome, run_agent_loop_trial, run_execution_template_trial};
use trial_simulator::load_simulator_context;

const K_RUN_UID: &str = "run_uid";
const K_TRIAL_UID: &str = "trial_uid";
const K_TRIAL_KEY: &str = "trial_key";
const K_STATUS: &str = "status";
const K_SESSION_ID: &str = "session_id";

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
    /// Parent workflow awakeable resolved when this trial workflow completes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_awakeable_id: Option<String>,
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
    /// Linked typed execution run.
    pub execution_run_uid: Option<Uuid>,
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

    /// Forwards cancellation to this trial's live child target work.
    #[shared]
    async fn request_cancel(signal: Json<ExperimentCancelSignal>) -> Result<(), HandlerError>;
}

/// Concrete behavior-lab trial workflow implementation.
pub struct ExperimentTrialRunImpl {
    pool: sqlx::PgPool,
    session_store: Arc<PostgresSessionStore>,
    providers: Arc<ProviderRegistry>,
    score_lineage: Option<ScoreLineageHandle>,
    config: Arc<MoaConfig>,
}

impl ExperimentTrialRunImpl {
    /// Creates a trial workflow with its durable stores and provider registry.
    ///
    /// `score_lineage` is `None` when the deployment selected a telemetry-only
    /// lineage sink. Trials still run, but they fail with a stable code instead
    /// of reporting evidence that was never stored.
    #[must_use]
    pub fn new(
        pool: sqlx::PgPool,
        session_store: Arc<PostgresSessionStore>,
        providers: Arc<ProviderRegistry>,
        score_lineage: Option<ScoreLineageHandle>,
        config: Arc<MoaConfig>,
    ) -> Self {
        Self {
            pool,
            session_store,
            providers,
            score_lineage,
            config,
        }
    }
}

impl ExperimentTrialRun for ExperimentTrialRunImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: dispatched only from authorized experiment execution paths.
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<ExperimentTrialRunWorkflowRequest>,
    ) -> Result<Json<ExperimentTrialRunStatusResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ExperimentTrialRun", "run");
        let request = request.into_inner();
        let expected_key = trial_workflow_key(request.trial.run_uid, &request.trial.trial_key);
        if expected_key != ctx.key() {
            return Err(TerminalError::new_with_code(404, "experiment trial key mismatch").into());
        }

        ctx.set(K_RUN_UID, Json(request.trial.run_uid));
        ctx.set(K_TRIAL_KEY, Json(request.trial.trial_key.clone()));
        annotate_trial_span(&request.trial, None);

        match run_trial(
            &ctx,
            request.clone(),
            self.config.as_ref(),
            &self.pool,
            &self.session_store,
            &self.providers,
            self.score_lineage.as_ref(),
        )
        .await
        {
            Ok(response) => {
                resolve_completion_awakeable(&ctx, &request);
                Ok(Json(response))
            }
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
                    &self.pool,
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
                resolve_completion_awakeable(&ctx, &request);
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ExperimentTrialRun", "status");
        let request = request.into_inner();
        let run_uid = ctx
            .get::<Json<Uuid>>(K_RUN_UID)
            .await?
            .map(Json::into_inner)
            .ok_or_else(|| TerminalError::new_with_code(404, "experiment trial not started"))?;
        let pool = self.pool.clone();
        Ok(ctx
            .run(|| async move {
                trial_status_response(pool, request, run_uid)
                    .await
                    .map(Json::from)
            })
            .name("experiment_trial_status")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, signal))]
    // SAFETY: control-only cancellation forward; the typed Session or Execution
    // cancellation handler enforces the child authority carried by this workflow.
    async fn request_cancel(
        &self,
        ctx: SharedWorkflowContext<'_>,
        signal: Json<ExperimentCancelSignal>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ExperimentTrialRun", "request_cancel");
        forward_child_cancellation(&ctx, signal.into_inner()).await
    }
}

fn resolve_completion_awakeable(
    ctx: &WorkflowContext<'_>,
    request: &ExperimentTrialRunWorkflowRequest,
) {
    if let Some(awakeable_id) = request.completion_awakeable_id.as_deref() {
        ctx.resolve_awakeable(awakeable_id, request.trial.trial_key.clone());
    }
}

/// Returns the stable Restate workflow key for a trial retry.
#[must_use]
pub fn trial_workflow_key(run_uid: Uuid, trial_key: &str) -> String {
    format!("{run_uid}:{trial_key}")
}

#[allow(
    clippy::too_many_arguments,
    reason = "the trial body keeps its durable stores and score sink explicit"
)]
async fn run_trial(
    ctx: &WorkflowContext<'_>,
    request: ExperimentTrialRunWorkflowRequest,
    config: &MoaConfig,
    pool: &sqlx::PgPool,
    session_store: &Arc<PostgresSessionStore>,
    providers: &Arc<ProviderRegistry>,
    score_lineage: Option<&ScoreLineageHandle>,
) -> Result<ExperimentTrialRunStatusResponse, HandlerError> {
    let trial = insert_or_load_trial(ctx, request.tenant_id, request.trial.clone(), pool).await?;
    ctx.set(K_TRIAL_UID, Json(trial.trial_uid));
    ctx.set(K_STATUS, Json(trial.status));
    annotate_trial_record_span(&trial);
    attach_current_trial_trace(ctx, trial.scope, trial.trial_uid, pool).await?;
    if !trial_status_allows_child_start(trial.status) {
        return status_response_from_record(request.tenant_id, trial);
    }
    if has_pending_cancellation(ctx, &trial.scope, trial.run_uid, pool).await? {
        return status_response_from_record(request.tenant_id, trial);
    }

    let trial = persist_trial_status(
        ctx,
        trial.scope,
        trial.trial_uid,
        ExperimentTrialStatus::Running,
        None,
        None,
        pool,
    )
    .await?;
    ctx.set(K_STATUS, Json(trial.status));

    let simulator_context = load_simulator_context(ctx, trial.clone(), pool).await?;
    let outcome = match trial.target_kind {
        ExperimentTargetKind::AgentLoop => {
            run_agent_loop_trial(
                ctx,
                request,
                trial.clone(),
                simulator_context,
                pool,
                session_store,
                providers,
            )
            .await?
        }
        ExperimentTargetKind::ExecutionTemplate => {
            run_execution_template_trial(ctx, request, trial.clone(), config, pool, session_store)
                .await?
        }
    };

    // Evaluation happens here, before any terminal status is persisted. The
    // target paths deliberately return evidence rather than writing a status
    // themselves, so there is exactly one place a trial can become terminal and
    // exactly one order in which that can happen.
    let TrialTargetOutcome {
        evidence,
        terminal_status,
        stop_reason,
        error,
    } = outcome;
    finalize_trial(
        ctx,
        TrialFinalization {
            trial: &trial,
            evidence,
            terminal_status,
            stop_reason,
            error,
        },
        score_lineage,
        pool,
    )
    .await
}

/// Builds the metadata for one agent-loop trial's own session.
///
/// The session is stamped with the owning trial's [`CallOrigin`] at creation.
/// That stamp is the only thing separating this session's tool calls from
/// production traffic: the trial drives an ordinary Session virtual object on
/// the ordinary process-wide tool router, so by the time a tool name reaches
/// policy evaluation the session record is the sole evidence that this is eval
/// traffic.
fn new_session_meta(
    tenant_id: TenantId,
    model: ModelId,
    identity: &Identity,
    call_origin: CallOrigin,
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
        call_origin,
        ..SessionMeta::default()
    })
}

fn session_actor_ref(identity: &Identity) -> Result<SessionActorRef, HandlerError> {
    match identity.identity_type {
        IdentityType::Operator | IdentityType::Agent => Ok(SessionActorRef::Identity {
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

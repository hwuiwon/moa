//! Restate workflow that executes one skill-backed procedure run.

use std::{collections::BTreeSet, sync::Arc};

use chrono::Utc;
use moa_artifacts::document::ArtifactDefinition;
use moa_artifacts::registry::{
    ArtifactNodeRunStatus, ArtifactNodeRunUpdate, ArtifactRegistry, ArtifactRun, ArtifactRunStatus,
    ArtifactRunUpdate, NewArtifactNodeRun,
};
use moa_core::traits::Identity;
use moa_core::wire::procedures::{
    ProcedureReviewDecisionKind, ProcedureReviewDecisionRequest, ProcedureReviewDecisionResponse,
    ProcedureSignalRequest, ProcedureSignalResponse,
};
use moa_core::{types::action_policy::ActionRuleScope, types::identifiers::TenantId};
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_skills::procedure::error::ProcedureError;
use moa_skills::procedure::interpreter::{
    ProcedureAdvance, ProcedureExecutionState, ProcedureInterpreter, ProcedureNodeRequest,
};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::workflows::errors::procedure_handler_error;
use crate::workflows::procedure_node_actions::{
    ProcedureNodeActionContext, ProcedureNodeActionOutcome, execute_procedure_node_action,
};
use moa_session::PostgresSessionStore;

const K_STATUS: &str = "status";
const K_RUN_UID: &str = "run_uid";
const K_CANCEL_REASON_PROMISE: &str = "cancel_reason";
const REVIEW_PROMISE_PREFIX: &str = "review";
const SIGNAL_PROMISE_PREFIX: &str = "signal";

/// Request payload for executing one procedure run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunProcedureRequest {
    /// Tenant that owns the procedure run.
    pub tenant_id: TenantId,
    /// Durable artifact run row identifier.
    pub run_uid: Uuid,
    /// Identity snapshot from the authorized caller.
    pub identity: Identity,
    /// Optional session associated with this procedure run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<moa_core::types::identifiers::SessionId>,
}

/// Terminal or current outcome for one procedure execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcedureOutcome {
    /// Procedure run row identifier.
    pub run_uid: Uuid,
    /// Current run status.
    pub status: String,
    /// Current node identifier, if execution has started.
    pub current_node_id: Option<String>,
    /// Terminal output payload, when completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    /// Terminal error payload, when failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Shared progress projection for an procedure invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcedureProgress {
    /// Procedure run row identifier.
    pub run_uid: Uuid,
    /// Current run status.
    pub status: String,
    /// Whether cancellation was requested through the procedure shared handler.
    pub cancel_requested: bool,
    /// Optional cancellation reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_reason: Option<String>,
}

/// Result of validating a procedure review decision before it is resolved into the live procedure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidatedProcedureReviewDecision {
    /// Public procedure review decision response.
    pub response: ProcedureReviewDecisionResponse,
    /// Decision payload to resolve into the running procedure.
    pub resolution: Option<ProcedureReviewResolution>,
}

/// Result of validating a procedure signal before it is resolved into the live procedure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidatedProcedureSignal {
    /// Public procedure signal response.
    pub response: ProcedureSignalResponse,
    /// Signal payload to resolve into the running procedure.
    pub resolution: Option<ProcedureSignalResolution>,
}

/// Typed review decision consumed by the running procedure body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcedureReviewResolution {
    /// Review node being decided.
    pub node_id: String,
    /// Decision to apply.
    pub decision: ProcedureReviewDecisionKind,
    /// Optional human-readable reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Optional approved output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
}

/// Typed external signal consumed by the running procedure body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcedureSignalResolution {
    /// Wait-signal node being resolved.
    pub node_id: String,
    /// Optional logical signal name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal_name: Option<String>,
    /// Signal payload.
    pub payload: Value,
}

/// Internal result of one durable procedure advancement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ProcedureStep {
    /// Execution has reached a stable run outcome.
    Outcome {
        /// Current run projection.
        outcome: ProcedureOutcome,
    },
    /// Execution must run one governed side-effect node before continuing.
    ExecuteNode {
        /// Current run projection before the side effect completes.
        outcome: ProcedureOutcome,
        /// Node-run row to update with the side-effect result.
        node_run_uid: Uuid,
        /// Side-effect request emitted by the pure interpreter.
        request: ProcedureNodeRequest,
    },
    /// Execution must run multiple branch side-effect nodes before continuing.
    ExecuteNodes {
        /// Current run projection before the side effects complete.
        outcome: ProcedureOutcome,
        /// Branch side-effect requests emitted by the pure interpreter.
        executions: Vec<ProcedureNodeExecution>,
    },
    /// Execution is durably waiting on a procedure review or signal.
    AwaitNode {
        /// Current run projection while blocked.
        outcome: ProcedureOutcome,
        /// Node-run row to update after the unblock event arrives.
        node_run_uid: Uuid,
        /// Blocked request emitted by the pure interpreter.
        request: ProcedureNodeRequest,
    },
}

/// One branch node execution selected by a procedure parallel node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ProcedureNodeExecution {
    /// Node-run row to update with the side-effect result.
    node_run_uid: Uuid,
    /// Side-effect request emitted by the pure interpreter.
    request: ProcedureNodeRequest,
}

/// Completed side-effect output for one branch node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ProcedureNodeActionResult {
    /// Node-run row to update.
    node_run_uid: Uuid,
    /// Side-effect request that was executed.
    request: ProcedureNodeRequest,
    /// Adapter outcome to persist.
    outcome: ProcedureNodeActionOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionResultMode {
    Single,
    Parallel,
}

impl ActionResultMode {
    fn records_failed_node(self) -> bool {
        matches!(self, Self::Parallel)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockedNodeKind {
    Review,
    Signal,
}

impl BlockedNodeKind {
    fn from_request(request: &ProcedureNodeRequest) -> Option<Self> {
        match request {
            ProcedureNodeRequest::Review { .. } => Some(Self::Review),
            ProcedureNodeRequest::WaitSignal { .. } => Some(Self::Signal),
            _ => None,
        }
    }

    fn promise_key(self, node_id: &str) -> String {
        match self {
            Self::Review => review_promise_key(node_id),
            Self::Signal => signal_promise_key(node_id),
        }
    }

    fn cancel_step_name(self, step_index: usize) -> String {
        match self {
            Self::Review => format!("procedure_cancel_while_review_{step_index}"),
            Self::Signal => format!("procedure_cancel_while_signal_{step_index}"),
        }
    }

    fn resolution_step_name(self, step_index: usize) -> String {
        match self {
            Self::Review => format!("procedure_review_resolution_{step_index}"),
            Self::Signal => format!("procedure_signal_resolution_{step_index}"),
        }
    }
}

enum ProcedureBlockedNodeResolution {
    Review(ProcedureReviewResolution),
    Signal(ProcedureSignalResolution),
}

/// Restate procedure execution surface for skill-backed procedure execution.
#[restate_sdk::workflow]
pub trait ProcedureExecution {
    /// Runs one skill-backed procedure execution.
    async fn run(
        request: Json<RunProcedureRequest>,
    ) -> Result<Json<ProcedureOutcome>, HandlerError>;

    /// Requests cancellation for the in-flight procedure run.
    #[shared]
    async fn request_cancel(reason: Json<String>) -> Result<(), HandlerError>;

    /// Resolves a pending procedure review node.
    #[shared]
    async fn decide_review(
        resolution: Json<ProcedureReviewResolution>,
    ) -> Result<Json<ProcedureReviewDecisionResponse>, HandlerError>;

    /// Resolves a pending procedure wait-signal node.
    #[shared]
    async fn signal(
        resolution: Json<ProcedureSignalResolution>,
    ) -> Result<Json<ProcedureSignalResponse>, HandlerError>;

    /// Reads lightweight procedure progress from Restate state.
    #[shared]
    async fn progress() -> Result<Json<ProcedureProgress>, HandlerError>;
}

/// Concrete procedure execution implementation.
#[derive(Clone)]
pub struct ProcedureExecutionImpl {
    registry: ArtifactRegistry,
    session_store: Arc<PostgresSessionStore>,
}

impl ProcedureExecutionImpl {
    /// Creates a procedure workflow with its artifact and session stores.
    #[must_use]
    pub fn new(registry: ArtifactRegistry, session_store: Arc<PostgresSessionStore>) -> Self {
        Self {
            registry,
            session_store,
        }
    }
}

impl ProcedureExecution for ProcedureExecutionImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<RunProcedureRequest>,
    ) -> Result<Json<ProcedureOutcome>, HandlerError> {
        annotate_restate_handler_span("ProcedureExecution", "run");
        let request = request.into_inner();
        if request.run_uid.to_string() != ctx.key() {
            return Err(TerminalError::new_with_code(404, "procedure run id mismatch").into());
        }

        ctx.set(K_RUN_UID, Json(request.run_uid));
        ctx.set(
            K_STATUS,
            Json(ArtifactRunStatus::Running.as_str().to_string()),
        );
        let initial_request = request.clone();
        let registry = self.registry.clone();
        let mut step = ctx
            .run(|| async move {
                advance_procedure(registry, initial_request)
                    .await
                    .map(Json::from)
            })
            .name("procedure_execute")
            .await?
            .into_inner();
        for step_index in 0..32 {
            match step {
                ProcedureStep::Outcome { outcome } => {
                    ctx.set(K_STATUS, Json(outcome.status.clone()));
                    return Ok(Json::from(outcome));
                }
                ProcedureStep::ExecuteNode {
                    node_run_uid,
                    request: node_request,
                    ..
                } => {
                    if let Some(cancel_step) = cancel_step_if_requested(
                        &ctx,
                        self.registry.clone(),
                        &request,
                        format!("procedure_cancel_before_node_{step_index}"),
                    )
                    .await?
                    {
                        step = cancel_step;
                        continue;
                    }
                    let action_outcomes = execute_node_actions(
                        &ctx,
                        self.session_store.clone(),
                        &request,
                        vec![ProcedureNodeExecution {
                            node_run_uid,
                            request: node_request,
                        }],
                    )
                    .await?;
                    let persist_request = request.clone();
                    let registry = self.registry.clone();
                    step = ctx
                        .run(|| async move {
                            persist_procedure_node_action_outcomes(
                                registry,
                                persist_request,
                                action_outcomes,
                                ActionResultMode::Single,
                            )
                            .await
                            .map(Json::from)
                        })
                        .name(format!("procedure_node_action_{step_index}"))
                        .await?
                        .into_inner();
                }
                ProcedureStep::ExecuteNodes { executions, .. } => {
                    if let Some(cancel_step) = cancel_step_if_requested(
                        &ctx,
                        self.registry.clone(),
                        &request,
                        format!("procedure_cancel_before_parallel_nodes_{step_index}"),
                    )
                    .await?
                    {
                        step = cancel_step;
                        continue;
                    }
                    let action_outcomes = execute_node_actions(
                        &ctx,
                        self.session_store.clone(),
                        &request,
                        executions,
                    )
                    .await?;
                    let persist_request = request.clone();
                    let registry = self.registry.clone();
                    step = ctx
                        .run(|| async move {
                            persist_procedure_node_action_outcomes(
                                registry,
                                persist_request,
                                action_outcomes,
                                ActionResultMode::Parallel,
                            )
                            .await
                            .map(Json::from)
                        })
                        .name(format!("procedure_parallel_node_actions_{step_index}"))
                        .await?
                        .into_inner();
                }
                ProcedureStep::AwaitNode {
                    outcome,
                    node_run_uid,
                    request: node_request,
                } => {
                    ctx.set(K_STATUS, Json(outcome.status));
                    step = await_blocked_node_resolution(
                        &ctx,
                        self.registry.clone(),
                        request.clone(),
                        node_run_uid,
                        node_request,
                        step_index,
                    )
                    .await?;
                }
            }
        }
        Err(TerminalError::new_with_code(
            400,
            "artifact procedure exceeded maximum effectful node steps",
        )
        .into())
    }

    #[tracing::instrument(skip(self, ctx, reason))]
    async fn request_cancel(
        &self,
        ctx: SharedWorkflowContext<'_>,
        reason: Json<String>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("ProcedureExecution", "request_cancel");
        ctx.resolve_promise(K_CANCEL_REASON_PROMISE, reason.into_inner());
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, resolution))]
    // SAFETY: called by the authorized Skills/decide_review service after tenant-admin authz.
    async fn decide_review(
        &self,
        ctx: SharedWorkflowContext<'_>,
        resolution: Json<ProcedureReviewResolution>,
    ) -> Result<Json<ProcedureReviewDecisionResponse>, HandlerError> {
        annotate_restate_handler_span("ProcedureExecution", "decide_review");
        let resolution = resolution.into_inner();
        ctx.resolve_promise(
            &review_promise_key(&resolution.node_id),
            Json::from(resolution.clone()),
        );
        Ok(Json::from(ProcedureReviewDecisionResponse {
            run_id: procedure_run_uid_from_key(ctx.key())?,
            accepted: true,
            status: ArtifactRunStatus::PendingReview.as_str().to_string(),
            current_node_id: Some(resolution.node_id),
        }))
    }

    #[tracing::instrument(skip(self, ctx, resolution))]
    // SAFETY: called by the authorized Skills/signal service after tenant-operator authz.
    async fn signal(
        &self,
        ctx: SharedWorkflowContext<'_>,
        resolution: Json<ProcedureSignalResolution>,
    ) -> Result<Json<ProcedureSignalResponse>, HandlerError> {
        annotate_restate_handler_span("ProcedureExecution", "signal");
        let resolution = resolution.into_inner();
        ctx.resolve_promise(
            &signal_promise_key(&resolution.node_id),
            Json::from(resolution.clone()),
        );
        Ok(Json::from(ProcedureSignalResponse {
            run_id: procedure_run_uid_from_key(ctx.key())?,
            accepted: true,
            status: ArtifactRunStatus::Running.as_str().to_string(),
            current_node_id: Some(resolution.node_id),
        }))
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn progress(
        &self,
        ctx: SharedWorkflowContext<'_>,
    ) -> Result<Json<ProcedureProgress>, HandlerError> {
        annotate_restate_handler_span("ProcedureExecution", "progress");
        let run_uid = ctx
            .get::<Json<Uuid>>(K_RUN_UID)
            .await?
            .map(Json::into_inner)
            .unwrap_or_else(|| Uuid::parse_str(ctx.key()).unwrap_or_else(|_| Uuid::nil()));
        let status = ctx
            .get::<Json<String>>(K_STATUS)
            .await?
            .map(Json::into_inner)
            .unwrap_or_else(|| ArtifactRunStatus::Queued.as_str().to_string());
        let cancel_reason = ctx
            .peek_promise::<String>(K_CANCEL_REASON_PROMISE)
            .await
            .map_err(HandlerError::from)?;
        Ok(Json::from(ProcedureProgress {
            run_uid,
            status,
            cancel_requested: cancel_reason.is_some(),
            cancel_reason,
        }))
    }
}

async fn cancel_step_if_requested(
    ctx: &WorkflowContext<'_>,
    registry: ArtifactRegistry,
    request: &RunProcedureRequest,
    run_step_name: String,
) -> Result<Option<ProcedureStep>, HandlerError> {
    let Some(reason) = cancel_requested(ctx).await? else {
        return Ok(None);
    };
    persist_cancel_step(ctx, registry, request.clone(), reason, run_step_name)
        .await
        .map(Some)
}

async fn persist_cancel_step(
    ctx: &WorkflowContext<'_>,
    registry: ArtifactRegistry,
    request: RunProcedureRequest,
    reason: String,
    run_step_name: String,
) -> Result<ProcedureStep, HandlerError> {
    Ok(ctx
        .run(|| async move {
            persist_procedure_cancel(registry, request, reason)
                .await
                .map(Json::from)
        })
        .name(run_step_name)
        .await?
        .into_inner())
}

/// Runs the effectful side of each traversed procedure node.
///
/// Parallel nodes express graph fan-out/join semantics for the interpreter;
/// their side effects currently execute sequentially here in a deterministic
/// order rather than concurrently.
async fn execute_node_actions(
    ctx: &WorkflowContext<'_>,
    session_store: Arc<PostgresSessionStore>,
    request: &RunProcedureRequest,
    executions: Vec<ProcedureNodeExecution>,
) -> Result<Vec<ProcedureNodeActionResult>, HandlerError> {
    let mut action_results = Vec::with_capacity(executions.len());
    for execution in executions {
        let action_outcome = execute_procedure_node_action(
            ctx,
            session_store.clone(),
            ProcedureNodeActionContext {
                tenant_id: request.tenant_id,
                run_uid: request.run_uid,
                node_id: blocked_node_id(&execution.request),
                session_id: request.session_id,
                identity: request.identity.clone(),
                cancel_promise_key: Some(K_CANCEL_REASON_PROMISE.to_string()),
            },
            execution.request.clone(),
        )
        .await?;
        action_results.push(ProcedureNodeActionResult {
            node_run_uid: execution.node_run_uid,
            request: execution.request,
            outcome: action_outcome,
        });
    }
    Ok(action_results)
}

async fn await_blocked_node_resolution(
    ctx: &WorkflowContext<'_>,
    registry: ArtifactRegistry,
    request: RunProcedureRequest,
    node_run_uid: Uuid,
    node_request: ProcedureNodeRequest,
    step_index: usize,
) -> Result<ProcedureStep, HandlerError> {
    let node_id = blocked_node_id(&node_request);
    let kind = BlockedNodeKind::from_request(&node_request).ok_or_else(|| {
        TerminalError::new_with_code(
            400,
            format!("procedure node `{node_id}` is not a resumable blocked node"),
        )
    })?;
    let cancel_step_name = kind.cancel_step_name(step_index);
    let resolution_step_name = kind.resolution_step_name(step_index);

    match kind {
        BlockedNodeKind::Review => {
            let review_key = kind.promise_key(&node_id);
            let step = restate_sdk::select! {
                reason = ctx.promise::<String>(K_CANCEL_REASON_PROMISE) => {
                    persist_cancel_step(ctx, registry.clone(), request.clone(), reason?, cancel_step_name).await?
                },
                resolution = ctx.promise::<Json<ProcedureReviewResolution>>(review_key.as_str()) => {
                    persist_blocked_node_resolution_step(
                        ctx,
                        registry.clone(),
                        request.clone(),
                        node_run_uid,
                        ProcedureBlockedNodeResolution::Review(resolution?.into_inner()),
                        resolution_step_name,
                    )
                    .await?
                }
            };
            Ok(step)
        }
        BlockedNodeKind::Signal => {
            let signal_key = kind.promise_key(&node_id);
            let step = restate_sdk::select! {
                reason = ctx.promise::<String>(K_CANCEL_REASON_PROMISE) => {
                    persist_cancel_step(ctx, registry.clone(), request.clone(), reason?, cancel_step_name).await?
                },
                resolution = ctx.promise::<Json<ProcedureSignalResolution>>(signal_key.as_str()) => {
                    persist_blocked_node_resolution_step(
                        ctx,
                        registry.clone(),
                        request.clone(),
                        node_run_uid,
                        ProcedureBlockedNodeResolution::Signal(resolution?.into_inner()),
                        resolution_step_name,
                    )
                    .await?
                }
            };
            Ok(step)
        }
    }
}

async fn persist_blocked_node_resolution_step(
    ctx: &WorkflowContext<'_>,
    registry: ArtifactRegistry,
    request: RunProcedureRequest,
    node_run_uid: Uuid,
    resolution: ProcedureBlockedNodeResolution,
    run_step_name: String,
) -> Result<ProcedureStep, HandlerError> {
    Ok(ctx
        .run(|| async move {
            match resolution {
                ProcedureBlockedNodeResolution::Review(resolution) => {
                    persist_procedure_review_resolution(registry, request, node_run_uid, resolution)
                        .await
                }
                ProcedureBlockedNodeResolution::Signal(resolution) => {
                    persist_procedure_signal_resolution(registry, request, node_run_uid, resolution)
                        .await
                }
            }
            .map(Json::from)
        })
        .name(run_step_name)
        .await?
        .into_inner())
}

async fn cancel_requested(ctx: &WorkflowContext<'_>) -> Result<Option<String>, HandlerError> {
    ctx.peek_promise::<String>(K_CANCEL_REASON_PROMISE)
        .await
        .map_err(HandlerError::from)
}

async fn persist_procedure_cancel(
    registry: ArtifactRegistry,
    request: RunProcedureRequest,
    reason: String,
) -> Result<ProcedureStep, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let run = registry
        .load_run(&scope, request.run_uid)
        .await
        .map_err(artifact_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "procedure run not found"))?;
    if matches!(
        run.status,
        ArtifactRunStatus::Completed | ArtifactRunStatus::Failed
    ) {
        return Ok(ProcedureStep::Outcome {
            outcome: outcome_from_run(&run),
        });
    }

    if let Some(node_id) = run.current_node_id.as_deref()
        && let Ok(node_run_uid) = latest_node_run_uid(&registry, &scope, run.run_uid, node_id).await
    {
        registry
            .update_node_run(
                &scope,
                node_run_uid,
                ArtifactNodeRunUpdate {
                    status: Some(ArtifactNodeRunStatus::Cancelled),
                    output: None,
                    error: Some(Some(reason.clone())),
                    completed_at: Some(Some(Utc::now())),
                },
            )
            .await
            .map_err(artifact_handler_error)?;
    }

    if run.status == ArtifactRunStatus::Cancelled {
        return Ok(ProcedureStep::Outcome {
            outcome: outcome_from_run(&run),
        });
    }

    let updated = registry
        .cancel_run(&scope, request.run_uid, Some(reason))
        .await
        .map_err(artifact_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "procedure run not found"))?;
    Ok(ProcedureStep::Outcome {
        outcome: outcome_from_run(&updated),
    })
}

async fn advance_procedure(
    registry: ArtifactRegistry,
    request: RunProcedureRequest,
) -> Result<ProcedureStep, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let run = registry
        .load_run(&scope, request.run_uid)
        .await
        .map_err(artifact_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "procedure run not found"))?;

    if matches!(
        run.status,
        ArtifactRunStatus::Completed
            | ArtifactRunStatus::Failed
            | ArtifactRunStatus::Cancelled
            | ArtifactRunStatus::PendingReview
    ) {
        return Ok(ProcedureStep::Outcome {
            outcome: outcome_from_run(&run),
        });
    }

    let definition = load_procedure_definition(&registry, &scope, &run).await?;
    let execution_state = procedure_state_from_run(&run)?;

    registry
        .update_run(
            &scope,
            run.run_uid,
            ArtifactRunUpdate {
                status: Some(ArtifactRunStatus::Running),
                current_node_id: Some(run.current_node_id.clone()),
                state: None,
                output: None,
                error: Some(None),
                completed_at: None,
            },
        )
        .await
        .map_err(artifact_handler_error)?;

    advance_and_persist(&registry, &scope, &run, &definition, execution_state).await
}

async fn persist_procedure_node_action_outcomes(
    registry: ArtifactRegistry,
    request: RunProcedureRequest,
    action_results: Vec<ProcedureNodeActionResult>,
    mode: ActionResultMode,
) -> Result<ProcedureStep, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let run = registry
        .load_run(&scope, request.run_uid)
        .await
        .map_err(artifact_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "procedure run not found"))?;
    if matches!(
        run.status,
        ArtifactRunStatus::Completed | ArtifactRunStatus::Failed | ArtifactRunStatus::Cancelled
    ) {
        return Ok(ProcedureStep::Outcome {
            outcome: outcome_from_run(&run),
        });
    }
    let definition = load_procedure_definition(&registry, &scope, &run).await?;
    let interpreter = ProcedureInterpreter::new(&definition);
    let mut state = procedure_state_from_run(&run)?;

    for action_result in action_results {
        let node_id = blocked_node_id(&action_result.request);
        match action_result.outcome {
            ProcedureNodeActionOutcome::Completed { output } => {
                complete_node_run(
                    &registry,
                    &scope,
                    action_result.node_run_uid,
                    output.clone(),
                )
                .await?;
                state = interpreter
                    .complete_blocked_node(state, &node_id, output)
                    .map_err(procedure_handler_error)?;
            }
            ProcedureNodeActionOutcome::Failed { error } => {
                fail_node_run(&registry, &scope, action_result.node_run_uid, error.clone()).await?;
                if mode.records_failed_node() {
                    state.failed_nodes.insert(node_id.clone());
                }
                return fail_procedure_run(&registry, &scope, &run, node_id, &state, error).await;
            }
            ProcedureNodeActionOutcome::Cancelled { reason } => {
                cancel_node_run(
                    &registry,
                    &scope,
                    action_result.node_run_uid,
                    reason.clone(),
                )
                .await?;
                return persist_procedure_cancel(registry, request, reason).await;
            }
        }
    }

    advance_and_persist(&registry, &scope, &run, &definition, state).await
}

async fn complete_node_run(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    node_run_uid: Uuid,
    output: Value,
) -> Result<(), HandlerError> {
    registry
        .update_node_run(
            scope,
            node_run_uid,
            ArtifactNodeRunUpdate {
                status: Some(ArtifactNodeRunStatus::Completed),
                output: Some(Some(output)),
                error: Some(None),
                completed_at: Some(Some(Utc::now())),
            },
        )
        .await
        .map_err(artifact_handler_error)?;
    Ok(())
}

async fn fail_node_run(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    node_run_uid: Uuid,
    error: String,
) -> Result<(), HandlerError> {
    registry
        .update_node_run(
            scope,
            node_run_uid,
            ArtifactNodeRunUpdate {
                status: Some(ArtifactNodeRunStatus::Failed),
                output: None,
                error: Some(Some(error)),
                completed_at: Some(Some(Utc::now())),
            },
        )
        .await
        .map_err(artifact_handler_error)?;
    Ok(())
}

async fn cancel_node_run(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    node_run_uid: Uuid,
    reason: String,
) -> Result<(), HandlerError> {
    registry
        .update_node_run(
            scope,
            node_run_uid,
            ArtifactNodeRunUpdate {
                status: Some(ArtifactNodeRunStatus::Cancelled),
                output: None,
                error: Some(Some(reason)),
                completed_at: Some(Some(Utc::now())),
            },
        )
        .await
        .map_err(artifact_handler_error)?;
    Ok(())
}

async fn fail_procedure_run(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    run: &ArtifactRun,
    node_id: String,
    state: &ProcedureExecutionState,
    error: String,
) -> Result<ProcedureStep, HandlerError> {
    let updated = registry
        .update_run(
            scope,
            run.run_uid,
            ArtifactRunUpdate {
                status: Some(ArtifactRunStatus::Failed),
                current_node_id: Some(Some(node_id)),
                state: Some(procedure_state_json(state)),
                output: None,
                error: Some(Some(error)),
                completed_at: Some(Some(Utc::now())),
            },
        )
        .await
        .map_err(artifact_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "procedure run not found"))?;
    Ok(ProcedureStep::Outcome {
        outcome: outcome_from_run(&updated),
    })
}

/// Validates an explicit procedure review-node decision before resolving the workflow promise.
pub(crate) async fn validate_procedure_review_decision(
    registry: ArtifactRegistry,
    request: ProcedureReviewDecisionRequest,
) -> Result<ValidatedProcedureReviewDecision, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let run = registry
        .load_run(&scope, request.run_id)
        .await
        .map_err(artifact_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "procedure run not found"))?;
    if run.status != ArtifactRunStatus::PendingReview {
        return Ok(ValidatedProcedureReviewDecision {
            response: ProcedureReviewDecisionResponse {
                run_id: run.run_uid,
                accepted: false,
                status: run.status.as_str().to_string(),
                current_node_id: run.current_node_id.clone(),
            },
            resolution: None,
        });
    }

    let node_id = request
        .node_id
        .clone()
        .or_else(|| run.current_node_id.clone())
        .ok_or_else(|| TerminalError::new_with_code(400, "procedure run has no review node"))?;
    let state = procedure_state_from_run(&run)?;
    if !matches!(
        state.blocked_nodes.get(&node_id),
        Some(ProcedureNodeRequest::Review { .. })
    ) {
        return Ok(ValidatedProcedureReviewDecision {
            response: ProcedureReviewDecisionResponse {
                run_id: run.run_uid,
                accepted: false,
                status: run.status.as_str().to_string(),
                current_node_id: run.current_node_id.clone(),
            },
            resolution: None,
        });
    }

    Ok(ValidatedProcedureReviewDecision {
        response: ProcedureReviewDecisionResponse {
            run_id: run.run_uid,
            accepted: true,
            status: run.status.as_str().to_string(),
            current_node_id: Some(node_id.clone()),
        },
        resolution: Some(ProcedureReviewResolution {
            node_id,
            decision: request.decision,
            reason: request.reason,
            output: request.output,
        }),
    })
}

/// Validates an external procedure signal before resolving the workflow promise.
pub(crate) async fn validate_procedure_signal(
    registry: ArtifactRegistry,
    request: ProcedureSignalRequest,
) -> Result<ValidatedProcedureSignal, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let run = registry
        .load_run(&scope, request.run_id)
        .await
        .map_err(artifact_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "procedure run not found"))?;
    if matches!(
        run.status,
        ArtifactRunStatus::Completed | ArtifactRunStatus::Failed | ArtifactRunStatus::Cancelled
    ) {
        return Ok(ValidatedProcedureSignal {
            response: ProcedureSignalResponse {
                run_id: run.run_uid,
                accepted: false,
                status: run.status.as_str().to_string(),
                current_node_id: run.current_node_id.clone(),
            },
            resolution: None,
        });
    }

    let node_id = request
        .node_id
        .clone()
        .or_else(|| run.current_node_id.clone())
        .ok_or_else(|| TerminalError::new_with_code(400, "procedure run has no signal node"))?;
    let state = procedure_state_from_run(&run)?;
    if !matches!(
        state.blocked_nodes.get(&node_id),
        Some(ProcedureNodeRequest::WaitSignal { .. })
    ) {
        return Ok(ValidatedProcedureSignal {
            response: ProcedureSignalResponse {
                run_id: run.run_uid,
                accepted: false,
                status: run.status.as_str().to_string(),
                current_node_id: run.current_node_id.clone(),
            },
            resolution: None,
        });
    }

    Ok(ValidatedProcedureSignal {
        response: ProcedureSignalResponse {
            run_id: run.run_uid,
            accepted: true,
            status: run.status.as_str().to_string(),
            current_node_id: Some(node_id.clone()),
        },
        resolution: Some(ProcedureSignalResolution {
            node_id,
            signal_name: request.signal_name,
            payload: request.payload,
        }),
    })
}

async fn persist_procedure_review_resolution(
    registry: ArtifactRegistry,
    request: RunProcedureRequest,
    node_run_uid: Uuid,
    resolution: ProcedureReviewResolution,
) -> Result<ProcedureStep, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let run = registry
        .load_run(&scope, request.run_uid)
        .await
        .map_err(artifact_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "procedure run not found"))?;
    if matches!(
        run.status,
        ArtifactRunStatus::Completed | ArtifactRunStatus::Failed | ArtifactRunStatus::Cancelled
    ) {
        return Ok(ProcedureStep::Outcome {
            outcome: outcome_from_run(&run),
        });
    }
    let definition = load_procedure_definition(&registry, &scope, &run).await?;
    let state = procedure_state_from_run(&run)?;
    if !matches!(
        state.blocked_nodes.get(&resolution.node_id),
        Some(ProcedureNodeRequest::Review { .. })
    ) {
        return Err(TerminalError::new_with_code(
            400,
            format!(
                "procedure node `{}` is not waiting for review",
                resolution.node_id
            ),
        )
        .into());
    }

    match resolution.decision {
        ProcedureReviewDecisionKind::Approved => {
            let output = resolution.output.unwrap_or_else(|| {
                json!({
                    "decision": "approved",
                    "reason": resolution.reason,
                })
            });
            complete_node_run(&registry, &scope, node_run_uid, output.clone()).await?;
            let resumed_state = ProcedureInterpreter::new(&definition)
                .complete_blocked_node(state, &resolution.node_id, output)
                .map_err(procedure_handler_error)?;
            advance_and_persist(&registry, &scope, &run, &definition, resumed_state).await
        }
        ProcedureReviewDecisionKind::Rejected => {
            let reason = resolution
                .reason
                .unwrap_or_else(|| "procedure review rejected".to_string());
            fail_node_run(&registry, &scope, node_run_uid, reason.clone()).await?;
            fail_procedure_run(&registry, &scope, &run, resolution.node_id, &state, reason).await
        }
    }
}

async fn persist_procedure_signal_resolution(
    registry: ArtifactRegistry,
    request: RunProcedureRequest,
    node_run_uid: Uuid,
    resolution: ProcedureSignalResolution,
) -> Result<ProcedureStep, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let run = registry
        .load_run(&scope, request.run_uid)
        .await
        .map_err(artifact_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "procedure run not found"))?;
    if matches!(
        run.status,
        ArtifactRunStatus::Completed | ArtifactRunStatus::Failed | ArtifactRunStatus::Cancelled
    ) {
        return Ok(ProcedureStep::Outcome {
            outcome: outcome_from_run(&run),
        });
    }
    let definition = load_procedure_definition(&registry, &scope, &run).await?;
    let state = procedure_state_from_run(&run)?;
    if !matches!(
        state.blocked_nodes.get(&resolution.node_id),
        Some(ProcedureNodeRequest::WaitSignal { .. })
    ) {
        return Err(TerminalError::new_with_code(
            400,
            format!(
                "procedure node `{}` is not waiting for a signal",
                resolution.node_id
            ),
        )
        .into());
    }

    let output = json!({
        "signal_name": resolution.signal_name,
        "payload": resolution.payload,
    });
    complete_node_run(&registry, &scope, node_run_uid, output.clone()).await?;
    let resumed_state = ProcedureInterpreter::new(&definition)
        .complete_blocked_node(state, &resolution.node_id, output)
        .map_err(procedure_handler_error)?;
    advance_and_persist(&registry, &scope, &run, &definition, resumed_state).await
}

async fn advance_and_persist(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    run: &ArtifactRun,
    definition: &moa_artifacts::procedure::ProcedureDefinition,
    execution_state: ProcedureExecutionState,
) -> Result<ProcedureStep, HandlerError> {
    match ProcedureInterpreter::new(definition).advance(execution_state) {
        Ok(ProcedureAdvance::Completed { state, output }) => {
            append_completed_node_runs(registry, scope, definition, &state, Some(&output)).await?;
            let updated = registry
                .update_run(
                    scope,
                    run.run_uid,
                    ArtifactRunUpdate {
                        status: Some(ArtifactRunStatus::Completed),
                        current_node_id: Some(state.current_node_id.clone()),
                        state: Some(procedure_state_json(&state)),
                        output: Some(Some(output.clone())),
                        error: Some(None),
                        completed_at: Some(Some(Utc::now())),
                    },
                )
                .await
                .map_err(artifact_handler_error)?
                .ok_or_else(|| TerminalError::new_with_code(404, "procedure run not found"))?;
            Ok(ProcedureStep::Outcome {
                outcome: outcome_from_run(&updated),
            })
        }
        Ok(ProcedureAdvance::Blocked { state, request }) => {
            persist_blocked_request(registry, scope, run, definition, state, request).await
        }
        Ok(ProcedureAdvance::Ready { state, requests }) => {
            append_completed_node_runs(registry, scope, definition, &state, None).await?;
            let should_execute =
                !requests.is_empty() && requests.iter().all(is_executable_adapter_request);
            if !requests.is_empty() && !should_execute {
                let node_ids = requests
                    .iter()
                    .map(blocked_node_id)
                    .collect::<Vec<_>>()
                    .join(", ");
                let message = format!(
                    "parallel procedure branches cannot wait on review or signal nodes in v1: {node_ids}"
                );
                let updated = registry
                    .update_run(
                        scope,
                        run.run_uid,
                        ArtifactRunUpdate {
                            status: Some(ArtifactRunStatus::Failed),
                            current_node_id: Some(state.current_node_id.clone()),
                            state: Some(procedure_state_json(&state)),
                            output: None,
                            error: Some(Some(message)),
                            completed_at: Some(Some(Utc::now())),
                        },
                    )
                    .await
                    .map_err(artifact_handler_error)?
                    .ok_or_else(|| TerminalError::new_with_code(404, "procedure run not found"))?;
                return Ok(ProcedureStep::Outcome {
                    outcome: outcome_from_run(&updated),
                });
            }
            let mut executions = Vec::with_capacity(requests.len());
            for request in requests {
                let node_run_uid = registry
                    .append_node_run(
                        scope,
                        NewArtifactNodeRun {
                            run_uid: run.run_uid,
                            node_id: blocked_node_id(&request),
                            status: ArtifactNodeRunStatus::Running,
                            input: blocked_input(&request),
                            output: None,
                            error: None,
                            completed_at: None,
                        },
                    )
                    .await
                    .map_err(artifact_handler_error)?;
                if should_execute {
                    executions.push(ProcedureNodeExecution {
                        node_run_uid,
                        request,
                    });
                }
            }
            let updated = registry
                .update_run(
                    scope,
                    run.run_uid,
                    ArtifactRunUpdate {
                        status: Some(ArtifactRunStatus::Running),
                        current_node_id: Some(state.current_node_id.clone()),
                        state: Some(procedure_state_json(&state)),
                        output: None,
                        error: Some(None),
                        completed_at: None,
                    },
                )
                .await
                .map_err(artifact_handler_error)?
                .ok_or_else(|| TerminalError::new_with_code(404, "procedure run not found"))?;
            let outcome = outcome_from_run(&updated);
            if should_execute {
                return Ok(ProcedureStep::ExecuteNodes {
                    outcome,
                    executions,
                });
            }
            Ok(ProcedureStep::Outcome { outcome })
        }
        Err(error) => {
            let message = error.to_string();
            let updated = registry
                .update_run(
                    scope,
                    run.run_uid,
                    ArtifactRunUpdate {
                        status: Some(ArtifactRunStatus::Failed),
                        current_node_id: Some(run.current_node_id.clone()),
                        state: None,
                        output: None,
                        error: Some(Some(message)),
                        completed_at: Some(Some(Utc::now())),
                    },
                )
                .await
                .map_err(artifact_handler_error)?
                .ok_or_else(|| TerminalError::new_with_code(404, "procedure run not found"))?;
            Ok(ProcedureStep::Outcome {
                outcome: outcome_from_run(&updated),
            })
        }
    }
}

async fn load_procedure_definition(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    run: &ArtifactRun,
) -> Result<moa_artifacts::procedure::ProcedureDefinition, HandlerError> {
    let revision_uid = run.revision_uid.ok_or_else(|| {
        TerminalError::new_with_code(400, "procedure run is missing revision_uid")
    })?;
    let revision = registry
        .load_revision(scope, revision_uid)
        .await
        .map_err(artifact_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "procedure revision not found"))?;
    match revision.document.definition {
        ArtifactDefinition::Skill(skill) => skill.procedure.ok_or_else(|| {
            TerminalError::new_with_code(400, "skill artifact does not define a procedure").into()
        }),
        _ => Err(TerminalError::new_with_code(400, "artifact revision is not a skill").into()),
    }
}

async fn persist_blocked_request(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    run: &ArtifactRun,
    definition: &moa_artifacts::procedure::ProcedureDefinition,
    state: ProcedureExecutionState,
    request: ProcedureNodeRequest,
) -> Result<ProcedureStep, HandlerError> {
    append_completed_node_runs(registry, scope, definition, &state, None).await?;
    let status = if is_review_request(&request) {
        ArtifactNodeRunStatus::PendingReview
    } else {
        ArtifactNodeRunStatus::Running
    };
    let run_status = if is_review_request(&request) {
        ArtifactRunStatus::PendingReview
    } else {
        ArtifactRunStatus::Running
    };
    let node_run_uid = registry
        .append_node_run(
            scope,
            NewArtifactNodeRun {
                run_uid: run.run_uid,
                node_id: blocked_node_id(&request),
                status,
                input: blocked_input(&request),
                output: None,
                error: None,
                completed_at: None,
            },
        )
        .await
        .map_err(artifact_handler_error)?;
    let updated = registry
        .update_run(
            scope,
            run.run_uid,
            ArtifactRunUpdate {
                status: Some(run_status),
                current_node_id: Some(state.current_node_id.clone()),
                state: Some(procedure_state_json(&state)),
                output: None,
                error: Some(None),
                completed_at: None,
            },
        )
        .await
        .map_err(artifact_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "procedure run not found"))?;
    let outcome = outcome_from_run(&updated);
    if is_executable_adapter_request(&request) {
        return Ok(ProcedureStep::ExecuteNode {
            outcome,
            node_run_uid,
            request,
        });
    }
    if is_review_request(&request) || is_signal_request(&request) {
        return Ok(ProcedureStep::AwaitNode {
            outcome,
            node_run_uid,
            request,
        });
    }
    Ok(ProcedureStep::Outcome { outcome })
}

async fn append_completed_node_runs(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    definition: &moa_artifacts::procedure::ProcedureDefinition,
    state: &ProcedureExecutionState,
    terminal_output: Option<&Value>,
) -> Result<(), HandlerError> {
    // `append_node_runs` dedupes against already-persisted rows inside its own
    // transaction, so no pre-list read is needed here. Only collapse duplicate
    // ids within this traversal locally before handing them off.
    let node_ids = traversed_node_ids(definition, state)?;
    let mut seen = BTreeSet::new();
    let mut node_runs = Vec::new();
    for node_id in node_ids {
        if !seen.insert(node_id.clone()) {
            continue;
        }
        let output = if state.current_node_id.as_deref() == Some(node_id.as_str()) {
            terminal_output.cloned()
        } else {
            None
        };
        let input = definition
            .nodes
            .iter()
            .find(|node| node.id == node_id)
            .map(|node| node.input.clone())
            .unwrap_or_else(|| json!({}));
        node_runs.push(NewArtifactNodeRun {
            run_uid: state.run_uid,
            node_id,
            status: ArtifactNodeRunStatus::Completed,
            input,
            output,
            error: None,
            completed_at: Some(Utc::now()),
        });
    }
    registry
        .append_node_runs(scope, node_runs)
        .await
        .map_err(artifact_handler_error)?;
    Ok(())
}

async fn latest_node_run_uid(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    run_uid: Uuid,
    node_id: &str,
) -> Result<Uuid, HandlerError> {
    registry
        .list_node_runs(scope, run_uid)
        .await
        .map_err(artifact_handler_error)?
        .into_iter()
        .rev()
        .find(|node_run| node_run.node_id == node_id)
        .map(|node_run| node_run.node_run_uid)
        .ok_or_else(|| TerminalError::new_with_code(404, "workflow node run not found").into())
}

fn traversed_node_ids(
    definition: &moa_artifacts::procedure::ProcedureDefinition,
    state: &ProcedureExecutionState,
) -> Result<Vec<String>, HandlerError> {
    let start = definition
        .nodes
        .iter()
        .find(|node| node.kind == moa_artifacts::procedure::ProcedureNodeKind::Start)
        .ok_or_else(|| procedure_handler_error(ProcedureError::MissingStartNode))?;
    let mut node_ids = vec![start.id.clone()];
    for edge_id in &state.traversed_edge_ids {
        let edge = definition
            .edges
            .iter()
            .find(|edge| {
                edge.id
                    .as_ref()
                    .map(|id| id == edge_id)
                    .unwrap_or_else(|| format!("{}->{}", edge.from, edge.to) == *edge_id)
            })
            .ok_or_else(|| {
                procedure_handler_error(ProcedureError::EdgeNotFound {
                    edge_id: edge_id.clone(),
                })
            })?;
        node_ids.push(edge.to.clone());
    }
    Ok(node_ids
        .into_iter()
        .filter(|node_id| state.completed_nodes.contains(node_id))
        .collect())
}

fn procedure_state_json(state: &ProcedureExecutionState) -> Value {
    let mut value = serde_json::to_value(state).unwrap_or_else(|_| json!({}));
    if let Value::Object(map) = &mut value {
        map.insert(
            "blocked_node_ids".to_string(),
            json!(state.blocked_nodes.keys().collect::<Vec<_>>()),
        );
    }
    value
}

fn procedure_state_from_run(run: &ArtifactRun) -> Result<ProcedureExecutionState, HandlerError> {
    if run.state.get("run_uid").is_some() {
        return serde_json::from_value::<ProcedureExecutionState>(run.state.clone()).map_err(
            |error| {
                TerminalError::new_with_code(400, format!("invalid workflow state: {error}")).into()
            },
        );
    }
    let mut state = ProcedureExecutionState::new(run.run_uid, run.input.clone());
    state.state = run.state.clone();
    state.current_node_id = run.current_node_id.clone();
    Ok(state)
}

fn is_executable_adapter_request(request: &ProcedureNodeRequest) -> bool {
    matches!(
        request,
        ProcedureNodeRequest::Action { .. }
            | ProcedureNodeRequest::Tool { .. }
            | ProcedureNodeRequest::SkillAction { .. }
            | ProcedureNodeRequest::Agent { .. }
            | ProcedureNodeRequest::Worker { .. }
            | ProcedureNodeRequest::MemoryRead { .. }
            | ProcedureNodeRequest::MemoryWrite { .. }
    )
}

fn is_review_request(request: &ProcedureNodeRequest) -> bool {
    matches!(request, ProcedureNodeRequest::Review { .. })
}

fn is_signal_request(request: &ProcedureNodeRequest) -> bool {
    matches!(request, ProcedureNodeRequest::WaitSignal { .. })
}

fn review_promise_key(node_id: &str) -> String {
    format!("{REVIEW_PROMISE_PREFIX}:{node_id}")
}

fn signal_promise_key(node_id: &str) -> String {
    format!("{SIGNAL_PROMISE_PREFIX}:{node_id}")
}

fn procedure_run_uid_from_key(key: &str) -> Result<Uuid, HandlerError> {
    Uuid::parse_str(key).map_err(|error| {
        TerminalError::new_with_code(400, format!("invalid workflow run id: {error}")).into()
    })
}

fn blocked_node_id(request: &ProcedureNodeRequest) -> String {
    match request {
        ProcedureNodeRequest::Action { node_id, .. }
        | ProcedureNodeRequest::Tool { node_id, .. }
        | ProcedureNodeRequest::SkillAction { node_id, .. }
        | ProcedureNodeRequest::Agent { node_id, .. }
        | ProcedureNodeRequest::Worker { node_id, .. }
        | ProcedureNodeRequest::Review { node_id, .. }
        | ProcedureNodeRequest::WaitSignal { node_id, .. }
        | ProcedureNodeRequest::MemoryRead { node_id, .. }
        | ProcedureNodeRequest::MemoryWrite { node_id, .. } => node_id.clone(),
    }
}

fn blocked_input(request: &ProcedureNodeRequest) -> Value {
    match request {
        ProcedureNodeRequest::Action { input, .. }
        | ProcedureNodeRequest::Tool { input, .. }
        | ProcedureNodeRequest::SkillAction { input, .. }
        | ProcedureNodeRequest::Agent { input, .. }
        | ProcedureNodeRequest::Worker { input, .. }
        | ProcedureNodeRequest::Review { input, .. }
        | ProcedureNodeRequest::WaitSignal { input, .. }
        | ProcedureNodeRequest::MemoryRead { input, .. }
        | ProcedureNodeRequest::MemoryWrite { input, .. } => input.clone(),
    }
}

fn outcome_from_run(run: &ArtifactRun) -> ProcedureOutcome {
    ProcedureOutcome {
        run_uid: run.run_uid,
        status: run.status.as_str().to_string(),
        current_node_id: run.current_node_id.clone(),
        output: run.output.clone(),
        error: run.error.clone(),
    }
}

fn artifact_handler_error(error: moa_core::error::MoaError) -> HandlerError {
    procedure_handler_error(ProcedureError::Artifact(error))
}

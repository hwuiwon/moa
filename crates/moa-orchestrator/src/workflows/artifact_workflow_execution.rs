//! Restate workflow that executes one artifact-backed workflow run.

use std::collections::BTreeSet;

use chrono::Utc;
use moa_artifacts::document::ArtifactDefinition;
use moa_artifacts::registry::{
    ArtifactNodeRunStatus, ArtifactNodeRunUpdate, ArtifactRegistry, ArtifactRun, ArtifactRunStatus,
    ArtifactRunUpdate, NewArtifactNodeRun,
};
use moa_core::traits::Identity;
use moa_core::wire::{
    WorkflowReviewDecisionKind, WorkflowReviewDecisionRequest, WorkflowReviewDecisionResponse,
    WorkflowSignalRequest, WorkflowSignalResponse,
};
use moa_core::{ActionRuleScope, TenantId};
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_workflows::error::WorkflowError;
use moa_workflows::interpreter::{
    WorkflowAdvance, WorkflowExecutionState, WorkflowInterpreter, WorkflowNodeRequest,
};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::workflows::errors::workflow_handler_error;
use crate::workflows::workflow_node_actions::{
    WorkflowNodeActionContext, WorkflowNodeActionOutcome, execute_workflow_node_action,
};

const K_STATUS: &str = "status";
const K_RUN_UID: &str = "run_uid";
const K_CANCEL_REASON_PROMISE: &str = "cancel_reason";
const REVIEW_PROMISE_PREFIX: &str = "review";
const SIGNAL_PROMISE_PREFIX: &str = "signal";

/// Request payload for executing one artifact workflow run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunArtifactWorkflowRequest {
    /// Tenant that owns the workflow run.
    pub tenant_id: TenantId,
    /// Durable artifact run row identifier.
    pub run_uid: Uuid,
    /// Identity snapshot from the authorized caller.
    pub identity: Identity,
    /// Optional session associated with this workflow run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<moa_core::SessionId>,
}

/// Terminal or current outcome for one artifact workflow execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactWorkflowOutcome {
    /// Workflow run row identifier.
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

/// Shared progress projection for an artifact workflow invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactWorkflowProgress {
    /// Workflow run row identifier.
    pub run_uid: Uuid,
    /// Current run status.
    pub status: String,
    /// Whether cancellation was requested through the workflow shared handler.
    pub cancel_requested: bool,
    /// Optional cancellation reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_reason: Option<String>,
}

/// Result of validating a workflow review decision before it is resolved into the live workflow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidatedWorkflowReviewDecision {
    /// Public workflow review decision response.
    pub response: WorkflowReviewDecisionResponse,
    /// Decision payload to resolve into the running workflow.
    pub resolution: Option<WorkflowReviewResolution>,
}

/// Result of validating a workflow signal before it is resolved into the live workflow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidatedWorkflowSignal {
    /// Public workflow signal response.
    pub response: WorkflowSignalResponse,
    /// Signal payload to resolve into the running workflow.
    pub resolution: Option<WorkflowSignalResolution>,
}

/// Typed review decision consumed by the running workflow body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowReviewResolution {
    /// Review node being decided.
    pub node_id: String,
    /// Decision to apply.
    pub decision: WorkflowReviewDecisionKind,
    /// Optional human-readable reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Optional approved output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
}

/// Typed external signal consumed by the running workflow body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowSignalResolution {
    /// Wait-signal node being resolved.
    pub node_id: String,
    /// Optional logical signal name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal_name: Option<String>,
    /// Signal payload.
    pub payload: Value,
}

/// Internal result of one durable artifact workflow advancement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ArtifactWorkflowStep {
    /// Execution has reached a stable run outcome.
    Outcome {
        /// Current run projection.
        outcome: ArtifactWorkflowOutcome,
    },
    /// Execution must run one governed side-effect node before continuing.
    ExecuteNode {
        /// Current run projection before the side effect completes.
        outcome: ArtifactWorkflowOutcome,
        /// Node-run row to update with the side-effect result.
        node_run_uid: Uuid,
        /// Side-effect request emitted by the pure interpreter.
        request: WorkflowNodeRequest,
    },
    /// Execution must run multiple branch side-effect nodes before continuing.
    ExecuteNodes {
        /// Current run projection before the side effects complete.
        outcome: ArtifactWorkflowOutcome,
        /// Branch side-effect requests emitted by the pure interpreter.
        executions: Vec<ArtifactWorkflowNodeExecution>,
    },
    /// Execution is durably waiting on a workflow review or signal.
    AwaitNode {
        /// Current run projection while blocked.
        outcome: ArtifactWorkflowOutcome,
        /// Node-run row to update after the unblock event arrives.
        node_run_uid: Uuid,
        /// Blocked request emitted by the pure interpreter.
        request: WorkflowNodeRequest,
    },
}

/// One branch node execution selected by a workflow parallel node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ArtifactWorkflowNodeExecution {
    /// Node-run row to update with the side-effect result.
    node_run_uid: Uuid,
    /// Side-effect request emitted by the pure interpreter.
    request: WorkflowNodeRequest,
}

/// Completed side-effect output for one branch node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ArtifactWorkflowNodeActionResult {
    /// Node-run row to update.
    node_run_uid: Uuid,
    /// Side-effect request that was executed.
    request: WorkflowNodeRequest,
    /// Adapter outcome to persist.
    outcome: WorkflowNodeActionOutcome,
}

/// Restate workflow surface for artifact-backed workflow execution.
#[restate_sdk::workflow]
pub trait ArtifactWorkflowExecution {
    /// Runs one artifact-backed workflow execution.
    async fn run(
        request: Json<RunArtifactWorkflowRequest>,
    ) -> Result<Json<ArtifactWorkflowOutcome>, HandlerError>;

    /// Requests cancellation for the in-flight artifact workflow run.
    #[shared]
    async fn request_cancel(reason: Json<String>) -> Result<(), HandlerError>;

    /// Resolves a pending workflow review node.
    #[shared]
    async fn decide_review(
        resolution: Json<WorkflowReviewResolution>,
    ) -> Result<Json<WorkflowReviewDecisionResponse>, HandlerError>;

    /// Resolves a pending workflow wait-signal node.
    #[shared]
    async fn signal(
        resolution: Json<WorkflowSignalResolution>,
    ) -> Result<Json<WorkflowSignalResponse>, HandlerError>;

    /// Reads lightweight workflow progress from Restate state.
    #[shared]
    async fn progress() -> Result<Json<ArtifactWorkflowProgress>, HandlerError>;
}

/// Concrete artifact workflow execution implementation.
pub struct ArtifactWorkflowExecutionImpl;

impl ArtifactWorkflowExecution for ArtifactWorkflowExecutionImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<RunArtifactWorkflowRequest>,
    ) -> Result<Json<ArtifactWorkflowOutcome>, HandlerError> {
        annotate_restate_handler_span("ArtifactWorkflowExecution", "run");
        let request = request.into_inner();
        if request.run_uid.to_string() != ctx.key() {
            return Err(TerminalError::new_with_code(404, "workflow run id mismatch").into());
        }

        ctx.set(K_RUN_UID, Json(request.run_uid));
        ctx.set(
            K_STATUS,
            Json(ArtifactRunStatus::Running.as_str().to_string()),
        );
        let initial_request = request.clone();
        let mut step = ctx
            .run(|| async move {
                advance_artifact_workflow(initial_request)
                    .await
                    .map(Json::from)
            })
            .name("artifact_workflow_execute")
            .await?
            .into_inner();
        for step_index in 0..32 {
            match step {
                ArtifactWorkflowStep::Outcome { outcome } => {
                    ctx.set(K_STATUS, Json(outcome.status.clone()));
                    return Ok(Json::from(outcome));
                }
                ArtifactWorkflowStep::ExecuteNode {
                    node_run_uid,
                    request: node_request,
                    ..
                } => {
                    if let Some(reason) = cancel_requested(&ctx).await? {
                        let cancel_request = request.clone();
                        step = ctx
                            .run(|| async move {
                                persist_workflow_cancel(cancel_request, reason)
                                    .await
                                    .map(Json::from)
                            })
                            .name(format!("artifact_workflow_cancel_before_node_{step_index}"))
                            .await?
                            .into_inner();
                        continue;
                    }
                    let node_id = blocked_node_id(&node_request);
                    let action_outcome = execute_workflow_node_action(
                        &ctx,
                        WorkflowNodeActionContext {
                            tenant_id: request.tenant_id,
                            run_uid: request.run_uid,
                            node_id,
                            session_id: request.session_id,
                            identity: request.identity.clone(),
                            cancel_promise_key: Some(K_CANCEL_REASON_PROMISE.to_string()),
                        },
                        node_request.clone(),
                    )
                    .await?;
                    let persist_request = request.clone();
                    step = ctx
                        .run(|| async move {
                            persist_workflow_node_action_outcome(
                                persist_request,
                                node_run_uid,
                                node_request,
                                action_outcome,
                            )
                            .await
                            .map(Json::from)
                        })
                        .name(format!("artifact_workflow_node_action_{step_index}"))
                        .await?
                        .into_inner();
                }
                ArtifactWorkflowStep::ExecuteNodes { executions, .. } => {
                    if let Some(reason) = cancel_requested(&ctx).await? {
                        let cancel_request = request.clone();
                        step = ctx
                            .run(|| async move {
                                persist_workflow_cancel(cancel_request, reason)
                                    .await
                                    .map(Json::from)
                            })
                            .name(format!(
                                "artifact_workflow_cancel_before_parallel_nodes_{step_index}"
                            ))
                            .await?
                            .into_inner();
                        continue;
                    }
                    let mut action_outcomes = Vec::with_capacity(executions.len());
                    for execution in executions {
                        let node_id = blocked_node_id(&execution.request);
                        let action_outcome = execute_workflow_node_action(
                            &ctx,
                            WorkflowNodeActionContext {
                                tenant_id: request.tenant_id,
                                run_uid: request.run_uid,
                                node_id,
                                session_id: request.session_id,
                                identity: request.identity.clone(),
                                cancel_promise_key: Some(K_CANCEL_REASON_PROMISE.to_string()),
                            },
                            execution.request.clone(),
                        )
                        .await?;
                        action_outcomes.push(ArtifactWorkflowNodeActionResult {
                            node_run_uid: execution.node_run_uid,
                            request: execution.request,
                            outcome: action_outcome,
                        });
                    }
                    let persist_request = request.clone();
                    step = ctx
                        .run(|| async move {
                            persist_workflow_node_action_outcomes(persist_request, action_outcomes)
                                .await
                                .map(Json::from)
                        })
                        .name(format!(
                            "artifact_workflow_parallel_node_actions_{step_index}"
                        ))
                        .await?
                        .into_inner();
                }
                ArtifactWorkflowStep::AwaitNode {
                    outcome,
                    node_run_uid,
                    request: node_request,
                } => {
                    ctx.set(K_STATUS, Json(outcome.status));
                    step = await_blocked_node_resolution(
                        &ctx,
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
            "artifact workflow exceeded maximum effectful node steps",
        )
        .into())
    }

    #[tracing::instrument(skip(self, ctx, reason))]
    async fn request_cancel(
        &self,
        ctx: SharedWorkflowContext<'_>,
        reason: Json<String>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("ArtifactWorkflowExecution", "request_cancel");
        ctx.resolve_promise(K_CANCEL_REASON_PROMISE, reason.into_inner());
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, resolution))]
    // SAFETY: called by the authorized Workflows/decide_review service after tenant-admin authz.
    async fn decide_review(
        &self,
        ctx: SharedWorkflowContext<'_>,
        resolution: Json<WorkflowReviewResolution>,
    ) -> Result<Json<WorkflowReviewDecisionResponse>, HandlerError> {
        annotate_restate_handler_span("ArtifactWorkflowExecution", "decide_review");
        let resolution = resolution.into_inner();
        ctx.resolve_promise(
            &review_promise_key(&resolution.node_id),
            Json::from(resolution.clone()),
        );
        Ok(Json::from(WorkflowReviewDecisionResponse {
            run_id: workflow_run_uid_from_key(ctx.key())?,
            accepted: true,
            status: ArtifactRunStatus::PendingReview.as_str().to_string(),
            current_node_id: Some(resolution.node_id),
        }))
    }

    #[tracing::instrument(skip(self, ctx, resolution))]
    // SAFETY: called by the authorized Workflows/signal service after tenant-operator authz.
    async fn signal(
        &self,
        ctx: SharedWorkflowContext<'_>,
        resolution: Json<WorkflowSignalResolution>,
    ) -> Result<Json<WorkflowSignalResponse>, HandlerError> {
        annotate_restate_handler_span("ArtifactWorkflowExecution", "signal");
        let resolution = resolution.into_inner();
        ctx.resolve_promise(
            &signal_promise_key(&resolution.node_id),
            Json::from(resolution.clone()),
        );
        Ok(Json::from(WorkflowSignalResponse {
            run_id: workflow_run_uid_from_key(ctx.key())?,
            accepted: true,
            status: ArtifactRunStatus::Running.as_str().to_string(),
            current_node_id: Some(resolution.node_id),
        }))
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn progress(
        &self,
        ctx: SharedWorkflowContext<'_>,
    ) -> Result<Json<ArtifactWorkflowProgress>, HandlerError> {
        annotate_restate_handler_span("ArtifactWorkflowExecution", "progress");
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
        Ok(Json::from(ArtifactWorkflowProgress {
            run_uid,
            status,
            cancel_requested: cancel_reason.is_some(),
            cancel_reason,
        }))
    }
}

async fn await_blocked_node_resolution(
    ctx: &WorkflowContext<'_>,
    request: RunArtifactWorkflowRequest,
    node_run_uid: Uuid,
    node_request: WorkflowNodeRequest,
    step_index: usize,
) -> Result<ArtifactWorkflowStep, HandlerError> {
    let node_id = blocked_node_id(&node_request);
    if is_review_request(&node_request) {
        let review_key = review_promise_key(&node_id);
        let step = restate_sdk::select! {
            reason = ctx.promise::<String>(K_CANCEL_REASON_PROMISE) => {
                let reason = reason?;
                let cancel_request = request.clone();
                ctx.run(|| async move {
                    persist_workflow_cancel(cancel_request, reason)
                        .await
                        .map(Json::from)
                })
                .name(format!("artifact_workflow_cancel_while_review_{step_index}"))
                .await?
                .into_inner()
            },
            resolution = ctx.promise::<Json<WorkflowReviewResolution>>(review_key.as_str()) => {
                let resolution = resolution?.into_inner();
                let persist_request = request.clone();
                ctx.run(|| async move {
                    persist_workflow_review_resolution(persist_request, node_run_uid, resolution)
                        .await
                        .map(Json::from)
                })
                .name(format!("artifact_workflow_review_resolution_{step_index}"))
                .await?
                .into_inner()
            }
        };
        return Ok(step);
    }

    if is_signal_request(&node_request) {
        let signal_key = signal_promise_key(&node_id);
        let step = restate_sdk::select! {
            reason = ctx.promise::<String>(K_CANCEL_REASON_PROMISE) => {
                let reason = reason?;
                let cancel_request = request.clone();
                ctx.run(|| async move {
                    persist_workflow_cancel(cancel_request, reason)
                        .await
                        .map(Json::from)
                })
                .name(format!("artifact_workflow_cancel_while_signal_{step_index}"))
                .await?
                .into_inner()
            },
            resolution = ctx.promise::<Json<WorkflowSignalResolution>>(signal_key.as_str()) => {
                let resolution = resolution?.into_inner();
                let persist_request = request.clone();
                ctx.run(|| async move {
                    persist_workflow_signal_resolution(persist_request, node_run_uid, resolution)
                        .await
                        .map(Json::from)
                })
                .name(format!("artifact_workflow_signal_resolution_{step_index}"))
                .await?
                .into_inner()
            }
        };
        return Ok(step);
    }

    Err(TerminalError::new_with_code(
        400,
        format!(
            "workflow node `{}` is not a resumable blocked node",
            blocked_node_id(&node_request)
        ),
    )
    .into())
}

async fn cancel_requested(ctx: &WorkflowContext<'_>) -> Result<Option<String>, HandlerError> {
    ctx.peek_promise::<String>(K_CANCEL_REASON_PROMISE)
        .await
        .map_err(HandlerError::from)
}

async fn persist_workflow_cancel(
    request: RunArtifactWorkflowRequest,
    reason: String,
) -> Result<ArtifactWorkflowStep, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let registry = ArtifactRegistry::new(OrchestratorCtx::current_graph_pool());
    let run = registry
        .load_run(&scope, request.run_uid)
        .await
        .map_err(artifact_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "workflow run not found"))?;
    if matches!(
        run.status,
        ArtifactRunStatus::Completed | ArtifactRunStatus::Failed
    ) {
        return Ok(ArtifactWorkflowStep::Outcome {
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
        return Ok(ArtifactWorkflowStep::Outcome {
            outcome: outcome_from_run(&run),
        });
    }

    let updated = registry
        .cancel_run(&scope, request.run_uid, Some(reason))
        .await
        .map_err(artifact_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "workflow run not found"))?;
    Ok(ArtifactWorkflowStep::Outcome {
        outcome: outcome_from_run(&updated),
    })
}

async fn advance_artifact_workflow(
    request: RunArtifactWorkflowRequest,
) -> Result<ArtifactWorkflowStep, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let registry = ArtifactRegistry::new(OrchestratorCtx::current_graph_pool());
    let run = registry
        .load_run(&scope, request.run_uid)
        .await
        .map_err(artifact_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "workflow run not found"))?;

    if matches!(
        run.status,
        ArtifactRunStatus::Completed
            | ArtifactRunStatus::Failed
            | ArtifactRunStatus::Cancelled
            | ArtifactRunStatus::PendingReview
    ) {
        return Ok(ArtifactWorkflowStep::Outcome {
            outcome: outcome_from_run(&run),
        });
    }

    let definition = load_workflow_definition(&registry, &scope, &run).await?;
    let execution_state = workflow_state_from_run(&run)?;

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

async fn persist_workflow_node_action_outcome(
    request: RunArtifactWorkflowRequest,
    node_run_uid: Uuid,
    node_request: WorkflowNodeRequest,
    action_outcome: WorkflowNodeActionOutcome,
) -> Result<ArtifactWorkflowStep, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let registry = ArtifactRegistry::new(OrchestratorCtx::current_graph_pool());
    let run = registry
        .load_run(&scope, request.run_uid)
        .await
        .map_err(artifact_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "workflow run not found"))?;
    if matches!(
        run.status,
        ArtifactRunStatus::Completed | ArtifactRunStatus::Failed | ArtifactRunStatus::Cancelled
    ) {
        return Ok(ArtifactWorkflowStep::Outcome {
            outcome: outcome_from_run(&run),
        });
    }
    let definition = load_workflow_definition(&registry, &scope, &run).await?;
    let state = workflow_state_from_run(&run)?;
    let node_id = blocked_node_id(&node_request);

    match action_outcome {
        WorkflowNodeActionOutcome::Completed { output } => {
            registry
                .update_node_run(
                    &scope,
                    node_run_uid,
                    ArtifactNodeRunUpdate {
                        status: Some(ArtifactNodeRunStatus::Completed),
                        output: Some(Some(output.clone())),
                        error: Some(None),
                        completed_at: Some(Some(Utc::now())),
                    },
                )
                .await
                .map_err(artifact_handler_error)?;
            let resumed_state = WorkflowInterpreter::new(&definition)
                .complete_blocked_node(state, &node_id, output)
                .map_err(workflow_handler_error)?;
            advance_and_persist(&registry, &scope, &run, &definition, resumed_state).await
        }
        WorkflowNodeActionOutcome::Failed { error } => {
            registry
                .update_node_run(
                    &scope,
                    node_run_uid,
                    ArtifactNodeRunUpdate {
                        status: Some(ArtifactNodeRunStatus::Failed),
                        output: None,
                        error: Some(Some(error.clone())),
                        completed_at: Some(Some(Utc::now())),
                    },
                )
                .await
                .map_err(artifact_handler_error)?;
            let updated = registry
                .update_run(
                    &scope,
                    run.run_uid,
                    ArtifactRunUpdate {
                        status: Some(ArtifactRunStatus::Failed),
                        current_node_id: Some(Some(node_id)),
                        state: Some(workflow_state_json(&state)),
                        output: None,
                        error: Some(Some(error)),
                        completed_at: Some(Some(Utc::now())),
                    },
                )
                .await
                .map_err(artifact_handler_error)?
                .ok_or_else(|| TerminalError::new_with_code(404, "workflow run not found"))?;
            Ok(ArtifactWorkflowStep::Outcome {
                outcome: outcome_from_run(&updated),
            })
        }
        WorkflowNodeActionOutcome::Cancelled { reason } => {
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
            persist_workflow_cancel(request, reason).await
        }
    }
}

async fn persist_workflow_node_action_outcomes(
    request: RunArtifactWorkflowRequest,
    action_results: Vec<ArtifactWorkflowNodeActionResult>,
) -> Result<ArtifactWorkflowStep, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let registry = ArtifactRegistry::new(OrchestratorCtx::current_graph_pool());
    let run = registry
        .load_run(&scope, request.run_uid)
        .await
        .map_err(artifact_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "workflow run not found"))?;
    if matches!(
        run.status,
        ArtifactRunStatus::Completed | ArtifactRunStatus::Failed | ArtifactRunStatus::Cancelled
    ) {
        return Ok(ArtifactWorkflowStep::Outcome {
            outcome: outcome_from_run(&run),
        });
    }
    let definition = load_workflow_definition(&registry, &scope, &run).await?;
    let interpreter = WorkflowInterpreter::new(&definition);
    let mut state = workflow_state_from_run(&run)?;

    for action_result in action_results {
        let node_id = blocked_node_id(&action_result.request);
        match action_result.outcome {
            WorkflowNodeActionOutcome::Completed { output } => {
                registry
                    .update_node_run(
                        &scope,
                        action_result.node_run_uid,
                        ArtifactNodeRunUpdate {
                            status: Some(ArtifactNodeRunStatus::Completed),
                            output: Some(Some(output.clone())),
                            error: Some(None),
                            completed_at: Some(Some(Utc::now())),
                        },
                    )
                    .await
                    .map_err(artifact_handler_error)?;
                state = interpreter
                    .complete_blocked_node(state, &node_id, output)
                    .map_err(workflow_handler_error)?;
            }
            WorkflowNodeActionOutcome::Failed { error } => {
                registry
                    .update_node_run(
                        &scope,
                        action_result.node_run_uid,
                        ArtifactNodeRunUpdate {
                            status: Some(ArtifactNodeRunStatus::Failed),
                            output: None,
                            error: Some(Some(error.clone())),
                            completed_at: Some(Some(Utc::now())),
                        },
                    )
                    .await
                    .map_err(artifact_handler_error)?;
                state.failed_nodes.insert(node_id.clone());
                let updated = registry
                    .update_run(
                        &scope,
                        run.run_uid,
                        ArtifactRunUpdate {
                            status: Some(ArtifactRunStatus::Failed),
                            current_node_id: Some(Some(node_id)),
                            state: Some(workflow_state_json(&state)),
                            output: None,
                            error: Some(Some(error)),
                            completed_at: Some(Some(Utc::now())),
                        },
                    )
                    .await
                    .map_err(artifact_handler_error)?
                    .ok_or_else(|| TerminalError::new_with_code(404, "workflow run not found"))?;
                return Ok(ArtifactWorkflowStep::Outcome {
                    outcome: outcome_from_run(&updated),
                });
            }
            WorkflowNodeActionOutcome::Cancelled { reason } => {
                registry
                    .update_node_run(
                        &scope,
                        action_result.node_run_uid,
                        ArtifactNodeRunUpdate {
                            status: Some(ArtifactNodeRunStatus::Cancelled),
                            output: None,
                            error: Some(Some(reason.clone())),
                            completed_at: Some(Some(Utc::now())),
                        },
                    )
                    .await
                    .map_err(artifact_handler_error)?;
                return persist_workflow_cancel(request, reason).await;
            }
        }
    }

    advance_and_persist(&registry, &scope, &run, &definition, state).await
}

/// Validates an explicit workflow review-node decision before resolving the workflow promise.
pub(crate) async fn validate_workflow_review_decision(
    request: WorkflowReviewDecisionRequest,
) -> Result<ValidatedWorkflowReviewDecision, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let registry = ArtifactRegistry::new(OrchestratorCtx::current_graph_pool());
    let run = registry
        .load_run(&scope, request.run_id)
        .await
        .map_err(artifact_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "workflow run not found"))?;
    if run.status != ArtifactRunStatus::PendingReview {
        return Ok(ValidatedWorkflowReviewDecision {
            response: WorkflowReviewDecisionResponse {
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
        .ok_or_else(|| TerminalError::new_with_code(400, "workflow run has no review node"))?;
    let state = workflow_state_from_run(&run)?;
    if !matches!(
        state.blocked_nodes.get(&node_id),
        Some(WorkflowNodeRequest::Review { .. })
    ) {
        return Ok(ValidatedWorkflowReviewDecision {
            response: WorkflowReviewDecisionResponse {
                run_id: run.run_uid,
                accepted: false,
                status: run.status.as_str().to_string(),
                current_node_id: run.current_node_id.clone(),
            },
            resolution: None,
        });
    }

    Ok(ValidatedWorkflowReviewDecision {
        response: WorkflowReviewDecisionResponse {
            run_id: run.run_uid,
            accepted: true,
            status: run.status.as_str().to_string(),
            current_node_id: Some(node_id.clone()),
        },
        resolution: Some(WorkflowReviewResolution {
            node_id,
            decision: request.decision,
            reason: request.reason,
            output: request.output,
        }),
    })
}

/// Validates an external workflow signal before resolving the workflow promise.
pub(crate) async fn validate_workflow_signal(
    request: WorkflowSignalRequest,
) -> Result<ValidatedWorkflowSignal, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let registry = ArtifactRegistry::new(OrchestratorCtx::current_graph_pool());
    let run = registry
        .load_run(&scope, request.run_id)
        .await
        .map_err(artifact_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "workflow run not found"))?;
    if matches!(
        run.status,
        ArtifactRunStatus::Completed | ArtifactRunStatus::Failed | ArtifactRunStatus::Cancelled
    ) {
        return Ok(ValidatedWorkflowSignal {
            response: WorkflowSignalResponse {
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
        .ok_or_else(|| TerminalError::new_with_code(400, "workflow run has no signal node"))?;
    let state = workflow_state_from_run(&run)?;
    if !matches!(
        state.blocked_nodes.get(&node_id),
        Some(WorkflowNodeRequest::WaitSignal { .. })
    ) {
        return Ok(ValidatedWorkflowSignal {
            response: WorkflowSignalResponse {
                run_id: run.run_uid,
                accepted: false,
                status: run.status.as_str().to_string(),
                current_node_id: run.current_node_id.clone(),
            },
            resolution: None,
        });
    }

    Ok(ValidatedWorkflowSignal {
        response: WorkflowSignalResponse {
            run_id: run.run_uid,
            accepted: true,
            status: run.status.as_str().to_string(),
            current_node_id: Some(node_id.clone()),
        },
        resolution: Some(WorkflowSignalResolution {
            node_id,
            signal_name: request.signal_name,
            payload: request.payload,
        }),
    })
}

async fn persist_workflow_review_resolution(
    request: RunArtifactWorkflowRequest,
    node_run_uid: Uuid,
    resolution: WorkflowReviewResolution,
) -> Result<ArtifactWorkflowStep, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let registry = ArtifactRegistry::new(OrchestratorCtx::current_graph_pool());
    let run = registry
        .load_run(&scope, request.run_uid)
        .await
        .map_err(artifact_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "workflow run not found"))?;
    if matches!(
        run.status,
        ArtifactRunStatus::Completed | ArtifactRunStatus::Failed | ArtifactRunStatus::Cancelled
    ) {
        return Ok(ArtifactWorkflowStep::Outcome {
            outcome: outcome_from_run(&run),
        });
    }
    let definition = load_workflow_definition(&registry, &scope, &run).await?;
    let state = workflow_state_from_run(&run)?;
    if !matches!(
        state.blocked_nodes.get(&resolution.node_id),
        Some(WorkflowNodeRequest::Review { .. })
    ) {
        return Err(TerminalError::new_with_code(
            400,
            format!(
                "workflow node `{}` is not waiting for review",
                resolution.node_id
            ),
        )
        .into());
    }

    match resolution.decision {
        WorkflowReviewDecisionKind::Approved => {
            let output = resolution.output.unwrap_or_else(|| {
                json!({
                    "decision": "approved",
                    "reason": resolution.reason,
                })
            });
            registry
                .update_node_run(
                    &scope,
                    node_run_uid,
                    ArtifactNodeRunUpdate {
                        status: Some(ArtifactNodeRunStatus::Completed),
                        output: Some(Some(output.clone())),
                        error: Some(None),
                        completed_at: Some(Some(Utc::now())),
                    },
                )
                .await
                .map_err(artifact_handler_error)?;
            let resumed_state = WorkflowInterpreter::new(&definition)
                .complete_blocked_node(state, &resolution.node_id, output)
                .map_err(workflow_handler_error)?;
            advance_and_persist(&registry, &scope, &run, &definition, resumed_state).await
        }
        WorkflowReviewDecisionKind::Rejected => {
            let reason = resolution
                .reason
                .unwrap_or_else(|| "workflow review rejected".to_string());
            registry
                .update_node_run(
                    &scope,
                    node_run_uid,
                    ArtifactNodeRunUpdate {
                        status: Some(ArtifactNodeRunStatus::Failed),
                        output: None,
                        error: Some(Some(reason.clone())),
                        completed_at: Some(Some(Utc::now())),
                    },
                )
                .await
                .map_err(artifact_handler_error)?;
            let updated = registry
                .update_run(
                    &scope,
                    run.run_uid,
                    ArtifactRunUpdate {
                        status: Some(ArtifactRunStatus::Failed),
                        current_node_id: Some(Some(resolution.node_id)),
                        state: Some(workflow_state_json(&state)),
                        output: None,
                        error: Some(Some(reason)),
                        completed_at: Some(Some(Utc::now())),
                    },
                )
                .await
                .map_err(artifact_handler_error)?
                .ok_or_else(|| TerminalError::new_with_code(404, "workflow run not found"))?;
            Ok(ArtifactWorkflowStep::Outcome {
                outcome: outcome_from_run(&updated),
            })
        }
    }
}

async fn persist_workflow_signal_resolution(
    request: RunArtifactWorkflowRequest,
    node_run_uid: Uuid,
    resolution: WorkflowSignalResolution,
) -> Result<ArtifactWorkflowStep, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let registry = ArtifactRegistry::new(OrchestratorCtx::current_graph_pool());
    let run = registry
        .load_run(&scope, request.run_uid)
        .await
        .map_err(artifact_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "workflow run not found"))?;
    if matches!(
        run.status,
        ArtifactRunStatus::Completed | ArtifactRunStatus::Failed | ArtifactRunStatus::Cancelled
    ) {
        return Ok(ArtifactWorkflowStep::Outcome {
            outcome: outcome_from_run(&run),
        });
    }
    let definition = load_workflow_definition(&registry, &scope, &run).await?;
    let state = workflow_state_from_run(&run)?;
    if !matches!(
        state.blocked_nodes.get(&resolution.node_id),
        Some(WorkflowNodeRequest::WaitSignal { .. })
    ) {
        return Err(TerminalError::new_with_code(
            400,
            format!(
                "workflow node `{}` is not waiting for a signal",
                resolution.node_id
            ),
        )
        .into());
    }

    let output = json!({
        "signal_name": resolution.signal_name,
        "payload": resolution.payload,
    });
    registry
        .update_node_run(
            &scope,
            node_run_uid,
            ArtifactNodeRunUpdate {
                status: Some(ArtifactNodeRunStatus::Completed),
                output: Some(Some(output.clone())),
                error: Some(None),
                completed_at: Some(Some(Utc::now())),
            },
        )
        .await
        .map_err(artifact_handler_error)?;
    let resumed_state = WorkflowInterpreter::new(&definition)
        .complete_blocked_node(state, &resolution.node_id, output)
        .map_err(workflow_handler_error)?;
    advance_and_persist(&registry, &scope, &run, &definition, resumed_state).await
}

async fn advance_and_persist(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    run: &ArtifactRun,
    definition: &moa_artifacts::workflow::WorkflowDefinition,
    execution_state: WorkflowExecutionState,
) -> Result<ArtifactWorkflowStep, HandlerError> {
    match WorkflowInterpreter::new(definition).advance(execution_state) {
        Ok(WorkflowAdvance::Completed { state, output }) => {
            append_completed_node_runs(registry, scope, definition, &state, Some(&output)).await?;
            let updated = registry
                .update_run(
                    scope,
                    run.run_uid,
                    ArtifactRunUpdate {
                        status: Some(ArtifactRunStatus::Completed),
                        current_node_id: Some(state.current_node_id.clone()),
                        state: Some(workflow_state_json(&state)),
                        output: Some(Some(output.clone())),
                        error: Some(None),
                        completed_at: Some(Some(Utc::now())),
                    },
                )
                .await
                .map_err(artifact_handler_error)?
                .ok_or_else(|| TerminalError::new_with_code(404, "workflow run not found"))?;
            Ok(ArtifactWorkflowStep::Outcome {
                outcome: outcome_from_run(&updated),
            })
        }
        Ok(WorkflowAdvance::Blocked { state, request }) => {
            persist_blocked_request(registry, scope, run, definition, state, request).await
        }
        Ok(WorkflowAdvance::Ready { state, requests }) => {
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
                    "parallel workflow branches cannot wait on review or signal nodes in v1: {node_ids}"
                );
                let updated = registry
                    .update_run(
                        scope,
                        run.run_uid,
                        ArtifactRunUpdate {
                            status: Some(ArtifactRunStatus::Failed),
                            current_node_id: Some(state.current_node_id.clone()),
                            state: Some(workflow_state_json(&state)),
                            output: None,
                            error: Some(Some(message)),
                            completed_at: Some(Some(Utc::now())),
                        },
                    )
                    .await
                    .map_err(artifact_handler_error)?
                    .ok_or_else(|| TerminalError::new_with_code(404, "workflow run not found"))?;
                return Ok(ArtifactWorkflowStep::Outcome {
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
                    executions.push(ArtifactWorkflowNodeExecution {
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
                        state: Some(workflow_state_json(&state)),
                        output: None,
                        error: Some(None),
                        completed_at: None,
                    },
                )
                .await
                .map_err(artifact_handler_error)?
                .ok_or_else(|| TerminalError::new_with_code(404, "workflow run not found"))?;
            let outcome = outcome_from_run(&updated);
            if should_execute {
                return Ok(ArtifactWorkflowStep::ExecuteNodes {
                    outcome,
                    executions,
                });
            }
            Ok(ArtifactWorkflowStep::Outcome { outcome })
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
                .ok_or_else(|| TerminalError::new_with_code(404, "workflow run not found"))?;
            Ok(ArtifactWorkflowStep::Outcome {
                outcome: outcome_from_run(&updated),
            })
        }
    }
}

async fn load_workflow_definition(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    run: &ArtifactRun,
) -> Result<moa_artifacts::workflow::WorkflowDefinition, HandlerError> {
    let revision_uid = run
        .revision_uid
        .ok_or_else(|| TerminalError::new_with_code(400, "workflow run is missing revision_uid"))?;
    let revision = registry
        .load_revision(scope, revision_uid)
        .await
        .map_err(artifact_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "workflow revision not found"))?;
    match revision.document.definition {
        ArtifactDefinition::Workflow(definition) => Ok(definition),
        _ => Err(TerminalError::new_with_code(400, "artifact revision is not a workflow").into()),
    }
}

async fn persist_blocked_request(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    run: &ArtifactRun,
    definition: &moa_artifacts::workflow::WorkflowDefinition,
    state: WorkflowExecutionState,
    request: WorkflowNodeRequest,
) -> Result<ArtifactWorkflowStep, HandlerError> {
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
                state: Some(workflow_state_json(&state)),
                output: None,
                error: Some(None),
                completed_at: None,
            },
        )
        .await
        .map_err(artifact_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "workflow run not found"))?;
    let outcome = outcome_from_run(&updated);
    if is_executable_adapter_request(&request) {
        return Ok(ArtifactWorkflowStep::ExecuteNode {
            outcome,
            node_run_uid,
            request,
        });
    }
    if is_review_request(&request) || is_signal_request(&request) {
        return Ok(ArtifactWorkflowStep::AwaitNode {
            outcome,
            node_run_uid,
            request,
        });
    }
    Ok(ArtifactWorkflowStep::Outcome { outcome })
}

async fn append_completed_node_runs(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    definition: &moa_artifacts::workflow::WorkflowDefinition,
    state: &WorkflowExecutionState,
    terminal_output: Option<&Value>,
) -> Result<(), HandlerError> {
    let mut existing_node_ids = registry
        .list_node_runs(scope, state.run_uid)
        .await
        .map_err(artifact_handler_error)?
        .into_iter()
        .map(|node_run| node_run.node_id)
        .collect::<BTreeSet<_>>();
    let node_ids = traversed_node_ids(definition, state)?;
    let mut node_runs = Vec::new();
    for node_id in node_ids {
        if existing_node_ids.contains(&node_id) {
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
            node_id: node_id.clone(),
            status: ArtifactNodeRunStatus::Completed,
            input,
            output,
            error: None,
            completed_at: Some(Utc::now()),
        });
        existing_node_ids.insert(node_id);
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
    definition: &moa_artifacts::workflow::WorkflowDefinition,
    state: &WorkflowExecutionState,
) -> Result<Vec<String>, HandlerError> {
    let start = definition
        .nodes
        .iter()
        .find(|node| node.kind == moa_artifacts::workflow::WorkflowNodeKind::Start)
        .ok_or_else(|| workflow_handler_error(WorkflowError::MissingStartNode))?;
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
                workflow_handler_error(WorkflowError::EdgeNotFound {
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

fn workflow_state_json(state: &WorkflowExecutionState) -> Value {
    let mut value = serde_json::to_value(state).unwrap_or_else(|_| json!({}));
    if let Value::Object(map) = &mut value {
        map.insert(
            "blocked_node_ids".to_string(),
            json!(state.blocked_nodes.keys().collect::<Vec<_>>()),
        );
    }
    value
}

fn workflow_state_from_run(run: &ArtifactRun) -> Result<WorkflowExecutionState, HandlerError> {
    if run.state.get("run_uid").is_some() {
        return serde_json::from_value::<WorkflowExecutionState>(run.state.clone()).map_err(
            |error| {
                TerminalError::new_with_code(400, format!("invalid workflow state: {error}")).into()
            },
        );
    }
    let mut state = WorkflowExecutionState::new(run.run_uid, run.input.clone());
    state.state = run.state.clone();
    state.current_node_id = run.current_node_id.clone();
    Ok(state)
}

fn is_executable_adapter_request(request: &WorkflowNodeRequest) -> bool {
    matches!(
        request,
        WorkflowNodeRequest::Action { .. }
            | WorkflowNodeRequest::Tool { .. }
            | WorkflowNodeRequest::SkillAction { .. }
            | WorkflowNodeRequest::Agent { .. }
            | WorkflowNodeRequest::SubAgent { .. }
            | WorkflowNodeRequest::MemoryRead { .. }
            | WorkflowNodeRequest::MemoryWrite { .. }
    )
}

fn is_review_request(request: &WorkflowNodeRequest) -> bool {
    matches!(request, WorkflowNodeRequest::Review { .. })
}

fn is_signal_request(request: &WorkflowNodeRequest) -> bool {
    matches!(request, WorkflowNodeRequest::WaitSignal { .. })
}

fn review_promise_key(node_id: &str) -> String {
    format!("{REVIEW_PROMISE_PREFIX}:{node_id}")
}

fn signal_promise_key(node_id: &str) -> String {
    format!("{SIGNAL_PROMISE_PREFIX}:{node_id}")
}

fn workflow_run_uid_from_key(key: &str) -> Result<Uuid, HandlerError> {
    Uuid::parse_str(key).map_err(|error| {
        TerminalError::new_with_code(400, format!("invalid workflow run id: {error}")).into()
    })
}

fn blocked_node_id(request: &WorkflowNodeRequest) -> String {
    match request {
        WorkflowNodeRequest::Action { node_id, .. }
        | WorkflowNodeRequest::Tool { node_id, .. }
        | WorkflowNodeRequest::SkillAction { node_id, .. }
        | WorkflowNodeRequest::Agent { node_id, .. }
        | WorkflowNodeRequest::SubAgent { node_id, .. }
        | WorkflowNodeRequest::Review { node_id, .. }
        | WorkflowNodeRequest::WaitSignal { node_id, .. }
        | WorkflowNodeRequest::MemoryRead { node_id, .. }
        | WorkflowNodeRequest::MemoryWrite { node_id, .. } => node_id.clone(),
    }
}

fn blocked_input(request: &WorkflowNodeRequest) -> Value {
    match request {
        WorkflowNodeRequest::Action { input, .. }
        | WorkflowNodeRequest::Tool { input, .. }
        | WorkflowNodeRequest::SkillAction { input, .. }
        | WorkflowNodeRequest::Agent { input, .. }
        | WorkflowNodeRequest::SubAgent { input, .. }
        | WorkflowNodeRequest::Review { input, .. }
        | WorkflowNodeRequest::WaitSignal { input, .. }
        | WorkflowNodeRequest::MemoryRead { input, .. }
        | WorkflowNodeRequest::MemoryWrite { input, .. } => input.clone(),
    }
}

fn outcome_from_run(run: &ArtifactRun) -> ArtifactWorkflowOutcome {
    ArtifactWorkflowOutcome {
        run_uid: run.run_uid,
        status: run.status.as_str().to_string(),
        current_node_id: run.current_node_id.clone(),
        output: run.output.clone(),
        error: run.error.clone(),
    }
}

fn artifact_handler_error(error: moa_core::MoaError) -> HandlerError {
    workflow_handler_error(WorkflowError::Artifact(error))
}

//! Typed exits produced by one bounded active task slice.

use std::collections::{BTreeMap, BTreeSet};

use moa_artifacts::execution_plan::{
    CapabilityReference, ExecutionFailureClass, ExecutionTaskOutcome, ExecutionTaskResult,
    ExecutionUsage,
};
use moa_core::{
    traits::SessionStore as _,
    types::{
        action_policy::{ActionRuleScope, CapabilityProvenance},
        completion::{CompletionContent, ToolCallContent, ToolInvocation},
        context::ContextMessage,
        identifiers::ToolCallId,
        resource::ResourceBudget,
        security::{
            SecurityCircuitOwner, SecurityCircuitStage, SecurityCircuitState, ToolCapabilityId,
        },
        tools::{AsyncToolJobTerminalOutcome, IdempotencyClass, ToolAsyncMode},
    },
};
use moa_execution::{
    capability::{CapabilitySource, ExecutionCapability},
    repository::task::{
        NewTaskAttemptCheckpoint, TaskAttemptCheckpointKind, TaskAttemptCheckpointRecord,
        TaskAttemptCheckpointWriteOutcome, TaskAttemptRecord,
    },
    schema::validate_instance,
    state::{LogicalTaskKind, completed_task_outcome, failed_task_outcome},
    wire::{ExecutionTaskAttemptRequest, ExecutionToolDispatchRejection},
};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    services::llm_gateway::{
        BoundedCompletionRequest, LLMCompletionAction, LLMCompletionOwner, LLMGatewayClient,
        attach_completion_owner, completion_idempotency_key,
    },
    tool_invocation::governed::{
        GovernedInvocationDisposition, GovernedInvocationOrigin, GovernedInvocationOutcome,
        GovernedInvocationRequest, invoke_governed_tool,
    },
    workflows::{
        durable_utc_now,
        errors::moa_error_to_handler_error,
        execution_task_attempt::{
            ExecutionTaskAttemptImpl, capability_tool_name, task_attempt_fence,
        },
    },
};

/// Current durable schema for a bounded task-agent continuation.
pub(super) const TASK_ATTEMPT_CONTINUATION_SCHEMA_VERSION: u32 = 1;

/// Maximum canonical continuation payload accepted by persistence.
pub(super) const MAX_TASK_ATTEMPT_CONTINUATION_BYTES: usize = 1024 * 1024;

/// Canonical state needed to resume an agent without replaying an external effect.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TaskAttemptContinuation {
    /// Durable schema version.
    pub schema_version: u32,
    /// Exact bounded execution state.
    pub state: TaskAttemptContinuationState,
    /// Exact storage-only action-review resolution consumed by the next attempt.
    pub review_resolution: Option<moa_execution::wire::ExecutionActionReviewResolution>,
    /// Exact terminal provider outcome consumed by a resumed agent external effect.
    pub external_job_resolution: Option<AsyncToolJobTerminalOutcome>,
    /// Release receipt that proves sandbox compute is asleep before this wait was published.
    pub workspace_release_receipt_id: Option<Uuid>,
}

/// Supported bounded continuation points.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum TaskAttemptContinuationState {
    /// Task-local agent state after a complete model/tool boundary.
    Agent {
        /// Complete bounded conversation required by the next model turn.
        messages: Vec<ContextMessage>,
        /// Zero-based model turn to execute next.
        next_turn: u32,
        /// Cumulative durable task usage.
        usage: moa_artifacts::execution_plan::ExecutionUsage,
        /// Prompt-injection circuit state owned by this exact task generation.
        security_circuit: SecurityCircuitState,
        /// Capabilities fenced by the persisted circuit.
        disabled_capabilities: std::collections::BTreeMap<String, ToolCapabilityId>,
        /// Exact effect waiting on a storage-only action review, when present.
        pending_review: Option<Box<PendingReviewedToolInvocation>>,
        /// Model-emitted tool effects not yet dispatched by a bounded slice.
        pending_tool_calls: Vec<ToolInvocation>,
        /// Exact agent tool invocation currently owned by an asynchronous provider job.
        pending_external: Option<PendingExternalToolInvocation>,
    },
    /// Direct capability effect waiting on a storage-only action review.
    CapabilityReview {
        /// Exact reviewed effect; resumption consumes its persisted resolution.
        pending_review: PendingReviewedToolInvocation,
        /// Cumulative durable task usage.
        usage: moa_artifacts::execution_plan::ExecutionUsage,
    },
    /// Direct async-capable effect reserved before its provider start.
    CapabilityExternalStart {
        /// Stable tool-call identity reused if recovery proves the provider did not start.
        tool_id: ToolCallId,
        /// Cumulative durable task usage before provider dispatch.
        usage: moa_artifacts::execution_plan::ExecutionUsage,
    },
}

/// Reviewed provider effect that must never be reconstructed from a fresh model turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PendingReviewedToolInvocation {
    /// Stable action-review identity.
    pub review_uid: Uuid,
    /// Exact durable review expiry returned by action-review admission.
    pub expires_at: chrono::DateTime<chrono::Utc>,
    /// Exact provider invocation accepted by policy.
    pub invocation: ToolInvocation,
    /// Compiler/catalog-pinned replay semantics for watchdog classification.
    pub effect_idempotency: IdempotencyClass,
}

/// Agent effect that was durably handed to an asynchronous provider.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PendingExternalToolInvocation {
    /// Stable MOA external-job identity bound before sandbox release.
    pub external_job_uid: Option<Uuid>,
    /// Exact model-emitted invocation awaiting the terminal provider result.
    pub invocation: ToolInvocation,
    /// Compiler/catalog-pinned replay semantics.
    pub effect_idempotency: IdempotencyClass,
}

struct AgentTaskSpec<'a> {
    instructions: &'a str,
    skill_refs: &'a [moa_artifacts::reference::ArtifactRef],
    capability_refs: &'a [CapabilityReference],
    max_turns: u32,
}

struct AgentPending {
    review: Option<PendingReviewedToolInvocation>,
    tool_calls: Vec<ToolInvocation>,
    external: Option<PendingExternalToolInvocation>,
}

impl TaskAttemptContinuation {
    /// Returns the exact action-review identity carried by a parked continuation.
    pub(super) const fn pending_review_uid(&self) -> Option<Uuid> {
        match &self.state {
            TaskAttemptContinuationState::Agent { pending_review, .. } => match pending_review {
                Some(pending) => Some(pending.review_uid),
                None => None,
            },
            TaskAttemptContinuationState::CapabilityReview { pending_review, .. } => {
                Some(pending_review.review_uid)
            }
            TaskAttemptContinuationState::CapabilityExternalStart { .. } => None,
        }
    }

    /// Binds the deterministic MOA external-job identity before checkpoint persistence.
    pub(super) fn bind_external_job(&mut self, external_job_uid: Uuid) -> Result<(), String> {
        let TaskAttemptContinuationState::Agent {
            pending_external: Some(pending),
            ..
        } = &mut self.state
        else {
            return Err("agent external continuation is missing its pending effect".to_string());
        };
        if pending
            .external_job_uid
            .is_some_and(|current| current != external_job_uid)
        {
            return Err("agent external continuation is bound to another job".to_string());
        }
        pending.external_job_uid = Some(external_job_uid);
        Ok(())
    }

    /// Serializes and enforces the hard continuation-size bound before any DB write.
    pub(super) fn to_bounded_json(&self) -> Result<serde_json::Value, String> {
        if self.schema_version != TASK_ATTEMPT_CONTINUATION_SCHEMA_VERSION {
            return Err(format!(
                "unsupported task continuation schema version {}",
                self.schema_version
            ));
        }
        let bytes = serde_json::to_vec(self)
            .map_err(|error| format!("serialize task continuation: {error}"))?;
        if bytes.len() > MAX_TASK_ATTEMPT_CONTINUATION_BYTES {
            return Err(format!(
                "task continuation is {} bytes; maximum is {} and the task must be decomposed or replanned",
                bytes.len(),
                MAX_TASK_ATTEMPT_CONTINUATION_BYTES
            ));
        }
        serde_json::from_slice(&bytes)
            .map_err(|error| format!("decode canonical task continuation: {error}"))
    }
}

/// Complete set of boundaries at which an active task workflow must return.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum ActiveTaskAttemptExit {
    /// A terminal, retryable, input, or replan outcome is ready for settlement.
    Outcome(ExecutionTaskOutcome),
    /// Action policy persisted a review; no workflow promise may remain live.
    ReviewPending {
        /// Exact bounded state written before releasing active ownership.
        continuation: TaskAttemptContinuation,
    },
    /// A complete model/tool boundary must resume in a freshly admitted attempt.
    Continue {
        /// Exact bounded state consumed by the next slice.
        continuation: TaskAttemptContinuation,
    },
    /// User input is required, with exact task-local agent state persisted before parking.
    InputPending {
        /// Canonical NeedsInput outcome written to the logical task.
        outcome: ExecutionTaskOutcome,
        /// Exact bounded state resumed after input settlement.
        continuation: TaskAttemptContinuation,
    },
    /// Provider work was committed and must resume outside this invocation.
    ExternalJob {
        /// MOA-owned job identity reserved before provider dispatch.
        external_job_uid: Uuid,
        /// Agent state to resume after the terminal provider callback.
        continuation: Option<TaskAttemptContinuation>,
    },
    /// Another durable owner fenced this attempt before its provider start.
    OwnershipLost,
}

/// Executes one admitted task without waiting on any future event.
pub(super) async fn execute_task_attempt(
    workflow: &ExecutionTaskAttemptImpl,
    ctx: &WorkflowContext<'_>,
    request: &ExecutionTaskAttemptRequest,
    started: &TaskAttemptRecord,
    checkpoint: Option<TaskAttemptCheckpointRecord>,
) -> Result<ActiveTaskAttemptExit, HandlerError> {
    let continuation = checkpoint
        .map(
            |checkpoint| -> Result<TaskAttemptContinuation, HandlerError> {
                if checkpoint.task_generation != started.task.generation
                    || checkpoint.controller_generation != started.run.controller_generation
                {
                    return Err(TerminalError::new("task continuation generation is stale").into());
                }
                serde_json::from_value::<TaskAttemptContinuation>(checkpoint.payload).map_err(
                    |error| TerminalError::new(format!("decode task continuation: {error}")).into(),
                )
            },
        )
        .transpose()?;
    match &started.task.kind {
        LogicalTaskKind::Output { value } => {
            let outcome = match validate_instance(
                &started.run.active_plan.definition.output_schema,
                value,
                "execution_task.output",
            ) {
                Ok(()) => completed_task_outcome(value.clone(), started.task.actual.clone()),
                Err(error) => failed_task_outcome(
                    ExecutionFailureClass::InvalidOutput,
                    error.to_string(),
                    started.task.actual.clone(),
                ),
            };
            Ok(ActiveTaskAttemptExit::Outcome(outcome))
        }
        LogicalTaskKind::Capability { reference } => {
            execute_direct_capability(
                workflow,
                ctx,
                request,
                started,
                reference,
                continuation.as_ref(),
            )
            .await
        }
        LogicalTaskKind::Agent {
            instructions,
            skill_refs,
            capability_refs,
            max_turns,
        } => {
            execute_agent_turn(
                workflow,
                ctx,
                request,
                started,
                AgentTaskSpec {
                    instructions,
                    skill_refs,
                    capability_refs,
                    max_turns: *max_turns,
                },
                continuation.as_ref(),
            )
            .await
        }
        LogicalTaskKind::CompletionVerifier {
            instructions,
            max_turns,
            ..
        } => {
            execute_agent_turn(
                workflow,
                ctx,
                request,
                started,
                AgentTaskSpec {
                    instructions,
                    skill_refs: &[],
                    capability_refs: &[],
                    max_turns: *max_turns,
                },
                continuation.as_ref(),
            )
            .await
        }
        LogicalTaskKind::Review { .. }
        | LogicalTaskKind::WaitSignal { .. }
        | LogicalTaskKind::WaitUntil { .. } => Err(TerminalError::new(
            "storage-only logical task was incorrectly admitted as an active attempt",
        )
        .into()),
    }
}

/// Durable step boundary at which an active attempt reports progress.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttemptHeartbeat {
    /// One model turn returned, so the following tool dispatch starts its own stall window.
    ModelTurn,
    /// One governed tool invocation returned, so sandbox release and continuation persistence
    /// start their own stall window.
    ToolCall,
}

impl AttemptHeartbeat {
    /// Deterministic journal step name for this boundary.
    const fn observation_step(self) -> &'static str {
        match self {
            Self::ModelTurn => "task_attempt_model_turn_progress_at",
            Self::ToolCall => "task_attempt_tool_call_progress_at",
        }
    }

    /// Deterministic journal step name for the persisted heartbeat.
    const fn write_step(self) -> &'static str {
        match self {
            Self::ModelTurn => "record_task_attempt_model_turn_progress",
            Self::ToolCall => "record_task_attempt_tool_call_progress",
        }
    }
}

/// Advances the active attempt's durable progress clock at one completed step boundary.
///
/// Only an attempt that currently owns active capacity is heartbeated; the repository call is
/// fenced on the exact dispatch and rejects a parked, superseded, or already-settled attempt,
/// so a waiting task can never appear to make progress. The observation timestamp is journaled
/// through `durable_utc_now` so replay reuses the recorded instant instead of a fresh clock
/// read, and the repository write itself is monotonic.
async fn record_attempt_heartbeat(
    workflow: &ExecutionTaskAttemptImpl,
    ctx: &WorkflowContext<'_>,
    request: &ExecutionTaskAttemptRequest,
    boundary: AttemptHeartbeat,
) -> Result<(), HandlerError> {
    let observed_at = durable_utc_now(ctx, boundary.observation_step()).await?;
    let repository = workflow.repository.clone();
    let fence = task_attempt_fence(request);
    ctx.run(|| async move {
        repository
            .record_task_attempt_progress(fence, observed_at)
            .await
            .map(Json::from)
            .map_err(crate::workflows::errors::execution_error_to_handler_error)
    })
    .name(boundary.write_step())
    .await?;
    Ok(())
}

async fn persist_external_start_checkpoint(
    workflow: &ExecutionTaskAttemptImpl,
    ctx: &WorkflowContext<'_>,
    request: &ExecutionTaskAttemptRequest,
    started: &TaskAttemptRecord,
    kind: TaskAttemptCheckpointKind,
    continuation: &TaskAttemptContinuation,
) -> Result<bool, HandlerError> {
    let checkpoint = NewTaskAttemptCheckpoint {
        fence: task_attempt_fence(request),
        task_generation: started.task.generation,
        kind,
        schema_version: continuation.schema_version,
        payload: continuation.to_bounded_json().map_err(TerminalError::new)?,
        workspace_release_receipt: None,
        created_at: durable_utc_now(ctx, "task_external_start_checkpointed_at").await?,
    };
    let repository = workflow.repository.clone();
    Ok(ctx
        .run(|| async move {
            repository
                .persist_running_task_external_start_checkpoint(checkpoint)
                .await
                .and_then(|outcome| match outcome {
                    TaskAttemptCheckpointWriteOutcome::Applied(_)
                    | TaskAttemptCheckpointWriteOutcome::Replayed(_) => Ok(Json::from(true)),
                    TaskAttemptCheckpointWriteOutcome::NotFound
                    | TaskAttemptCheckpointWriteOutcome::Stale => Ok(Json::from(false)),
                    TaskAttemptCheckpointWriteOutcome::InvalidState => {
                        Err(moa_execution::Error::InvalidRepositoryData {
                            message: "active external-start checkpoint was rejected".to_string(),
                        })
                    }
                })
                .map_err(crate::workflows::errors::execution_error_to_handler_error)
        })
        .name("persist_task_external_start_checkpoint")
        .await?
        .into_inner())
}

async fn execute_direct_capability(
    workflow: &ExecutionTaskAttemptImpl,
    ctx: &WorkflowContext<'_>,
    request: &ExecutionTaskAttemptRequest,
    started: &TaskAttemptRecord,
    reference: &CapabilityReference,
    continuation: Option<&TaskAttemptContinuation>,
) -> Result<ActiveTaskAttemptExit, HandlerError> {
    let capability = find_capability(&started.run, reference)?;
    let (tool_id, mut usage) = match continuation {
        Some(
            continuation @ TaskAttemptContinuation {
                state: TaskAttemptContinuationState::CapabilityReview { .. },
                ..
            },
        ) => return resume_reviewed_capability(capability, continuation),
        Some(TaskAttemptContinuation {
            state: TaskAttemptContinuationState::CapabilityExternalStart { tool_id, usage },
            review_resolution: None,
            external_job_resolution: None,
            ..
        }) => (*tool_id, usage.clone()),
        Some(_) => {
            return Err(TerminalError::new(
                "direct capability received an incompatible continuation",
            )
            .into());
        }
        None => {
            if let Err(error) = validate_instance(
                &capability.input_schema,
                &started.task.input,
                "execution_task.capability_input",
            ) {
                return Ok(ActiveTaskAttemptExit::Outcome(failed_task_outcome(
                    ExecutionFailureClass::InvalidInput,
                    error.to_string(),
                    started.task.actual.clone(),
                )));
            }
            (
                ToolCallId(Uuid::new_v5(
                    &request.dispatch_uid,
                    format!("task-capability:{}", started.task.generation).as_bytes(),
                )),
                started.task.actual.clone(),
            )
        }
    };
    let session = load_session(workflow, ctx, &started.run, &started.task).await?;
    let tool_name = capability_tool_name(capability)?;
    let tool_call = ToolCallContent {
        invocation: ToolInvocation {
            id: Some(tool_id.to_string()),
            name: tool_name.clone(),
            input: started.task.input.clone(),
        },
        provider_metadata: None,
    };
    let allowed_tools = BTreeSet::from([tool_name]);
    let provenance = CapabilityProvenance {
        kind: Some(capability_source_kind(&capability.source).to_string()),
        id: Some(format!(
            "{}@{}",
            capability.reference.name, capability.reference.version
        )),
        step_id: Some(started.task.node_id.clone()),
    };
    if matches!(
        capability.async_mode,
        ToolAsyncMode::MayReturnExternalJob { .. }
    ) {
        let provisional = TaskAttemptContinuation {
            schema_version: TASK_ATTEMPT_CONTINUATION_SCHEMA_VERSION,
            state: TaskAttemptContinuationState::CapabilityExternalStart {
                tool_id,
                usage: usage.clone(),
            },
            review_resolution: None,
            external_job_resolution: None,
            workspace_release_receipt_id: None,
        };
        if !persist_external_start_checkpoint(
            workflow,
            ctx,
            request,
            started,
            TaskAttemptCheckpointKind::CapabilityExternalStart,
            &provisional,
        )
        .await?
        {
            return Ok(ActiveTaskAttemptExit::OwnershipLost);
        }
    }
    let governed = invoke_governed_tool(
        ctx,
        GovernedInvocationRequest {
            session: &session,
            identity: &started.run.admitted_identity,
            session_id: started.run.session_id,
            tool_id,
            tool_call: &tool_call,
            allowed_tools: &allowed_tools,
            expected_tool_contract_revision: Some(&capability.contract_revision),
            active_canary: None,
            trusted_sandbox_manifest: None,
            origin: GovernedInvocationOrigin::ExecutionTask {
                run_uid: started.run.run_uid,
                task_uid: started.task.task_id.as_uuid(),
                generation: started.task.generation,
                attempt_generation: request.attempt_generation,
            },
            capability_provenance: Some(&provenance),
            capability_policy_context: Some(&capability.policy_context),
            resource_budget: ResourceBudget::until(request.attempt_deadline_at),
        },
        &workflow.session_limits,
        workflow.session_store.clone(),
        workflow.channel_adapters.as_ref(),
    )
    .await?;
    record_attempt_heartbeat(workflow, ctx, request, AttemptHeartbeat::ToolCall).await?;
    usage.tool_calls = usage.tool_calls.saturating_add(1);
    classify_capability_outcome(capability, governed, usage)
}

fn classify_capability_outcome(
    capability: &ExecutionCapability,
    outcome: GovernedInvocationOutcome,
    mut usage: ExecutionUsage,
) -> Result<ActiveTaskAttemptExit, HandlerError> {
    match outcome {
        GovernedInvocationOutcome::Completed(result)
            if result.disposition == GovernedInvocationDisposition::ReviewPending =>
        {
            let review = result.review.ok_or_else(|| {
                TerminalError::new(
                    "review-pending governed result is missing durable review identity",
                )
            })?;
            Ok(ActiveTaskAttemptExit::ReviewPending {
                continuation: TaskAttemptContinuation {
                    schema_version: TASK_ATTEMPT_CONTINUATION_SCHEMA_VERSION,
                    state: TaskAttemptContinuationState::CapabilityReview {
                        pending_review: PendingReviewedToolInvocation {
                            review_uid: review.review_uid,
                            expires_at: review.expires_at,
                            invocation: result.invocation,
                            effect_idempotency: capability.idempotency_class,
                        },
                        usage,
                    },
                    review_resolution: None,
                    external_job_resolution: None,
                    workspace_release_receipt_id: None,
                },
            })
        }
        GovernedInvocationOutcome::Completed(result) => {
            usage.retrieved_bytes = usage.retrieved_bytes.saturating_add(serialized_len(
                &result.output.safe_output.structured_payload(),
            ));
            let task_outcome = if result.output.is_error() {
                if capability.idempotency_class == IdempotencyClass::Idempotent {
                    failed_task_outcome(
                        ExecutionFailureClass::Retryable,
                        result.output.safe_output.to_text(),
                        usage,
                    )
                } else if capability.action_class
                    != moa_core::types::action_policy::ActionClass::Read
                {
                    ExecutionTaskOutcome {
                        schema_version: 1,
                        usage,
                        result: ExecutionTaskResult::UnknownOutcome {
                            message: format!(
                                "non-idempotent side effect returned an error after possible commit: {}",
                                result.output.safe_output.to_text()
                            ),
                        },
                    }
                } else {
                    failed_task_outcome(
                        ExecutionFailureClass::Terminal,
                        result.output.safe_output.to_text(),
                        usage,
                    )
                }
            } else {
                let value = result
                    .output
                    .safe_output
                    .structured_payload()
                    .cloned()
                    .unwrap_or_else(|| Value::String(result.output.safe_output.to_text()));
                if let Err(error) = validate_instance(
                    &capability.output_schema,
                    &value,
                    "execution_task.capability_output",
                ) {
                    if capability.action_class == moa_core::types::action_policy::ActionClass::Read
                    {
                        failed_task_outcome(
                            ExecutionFailureClass::InvalidOutput,
                            error.to_string(),
                            usage,
                        )
                    } else {
                        ExecutionTaskOutcome {
                            schema_version: 1,
                            usage,
                            result: ExecutionTaskResult::UnknownOutcome {
                                message: format!(
                                    "side effect returned invalid output after possible commit: {error}"
                                ),
                            },
                        }
                    }
                } else {
                    completed_task_outcome(value, usage)
                }
            };
            Ok(ActiveTaskAttemptExit::Outcome(task_outcome))
        }
        GovernedInvocationOutcome::ExternalJob {
            external_job_uid, ..
        } => Ok(ActiveTaskAttemptExit::ExternalJob {
            external_job_uid,
            continuation: None,
        }),
        GovernedInvocationOutcome::UnknownOutcome { message, .. } => {
            Ok(ActiveTaskAttemptExit::Outcome(ExecutionTaskOutcome {
                schema_version: 1,
                usage,
                result: ExecutionTaskResult::UnknownOutcome { message },
            }))
        }
        GovernedInvocationOutcome::NotDispatched { reason, .. } => {
            Ok(ActiveTaskAttemptExit::Outcome(failed_task_outcome(
                ExecutionFailureClass::Terminal,
                execution_dispatch_rejection_message(reason),
                usage,
            )))
        }
        GovernedInvocationOutcome::Delegation { .. } => {
            Err(TerminalError::new("execution tasks cannot invoke delegation capabilities").into())
        }
    }
}

fn resume_reviewed_capability(
    capability: &ExecutionCapability,
    continuation: &TaskAttemptContinuation,
) -> Result<ActiveTaskAttemptExit, HandlerError> {
    let TaskAttemptContinuationState::CapabilityReview {
        pending_review: _,
        usage,
    } = &continuation.state
    else {
        return Err(TerminalError::new(
            "direct capability received an incompatible agent continuation",
        )
        .into());
    };
    let resolution = continuation.review_resolution.as_ref().ok_or_else(|| {
        TerminalError::new("reviewed capability continuation has no durable resolution")
    })?;
    let exit = match resolution {
        moa_execution::wire::ExecutionActionReviewResolution::Completed { tool_output } => {
            match serde_json::from_value::<moa_core::types::tools::SecuredToolOutput>(
                tool_output.clone(),
            ) {
                Ok(output) => ActiveTaskAttemptExit::Outcome(capability_output_outcome(
                    capability,
                    output,
                    usage.clone(),
                )),
                Err(error) => ActiveTaskAttemptExit::Outcome(ExecutionTaskOutcome {
                    schema_version: 1,
                    usage: usage.clone(),
                    result: ExecutionTaskResult::UnknownOutcome {
                        message: format!(
                            "reviewed capability returned invalid output after possible commit: {error}"
                        ),
                    },
                }),
            }
        }
        moa_execution::wire::ExecutionActionReviewResolution::ExternalJob {
            external_job_uid,
            ..
        } => ActiveTaskAttemptExit::ExternalJob {
            external_job_uid: *external_job_uid,
            continuation: None,
        },
        moa_execution::wire::ExecutionActionReviewResolution::Failed { class, message } => {
            ActiveTaskAttemptExit::Outcome(ExecutionTaskOutcome {
                schema_version: 1,
                usage: usage.clone(),
                result: ExecutionTaskResult::Failed {
                    class: class.clone(),
                    message: message.clone(),
                },
            })
        }
        moa_execution::wire::ExecutionActionReviewResolution::UnknownOutcome { message } => {
            ActiveTaskAttemptExit::Outcome(ExecutionTaskOutcome {
                schema_version: 1,
                usage: usage.clone(),
                result: ExecutionTaskResult::UnknownOutcome {
                    message: message.clone(),
                },
            })
        }
        moa_execution::wire::ExecutionActionReviewResolution::NotDispatched { reason } => {
            ActiveTaskAttemptExit::Outcome(failed_task_outcome(
                ExecutionFailureClass::Terminal,
                execution_dispatch_rejection_message(*reason),
                usage.clone(),
            ))
        }
        moa_execution::wire::ExecutionActionReviewResolution::Denied { reason } => {
            ActiveTaskAttemptExit::Outcome(failed_task_outcome(
                ExecutionFailureClass::AuthorizationDenied,
                reason.clone(),
                usage.clone(),
            ))
        }
        moa_execution::wire::ExecutionActionReviewResolution::TimedOut { reason } => {
            ActiveTaskAttemptExit::Outcome(failed_task_outcome(
                ExecutionFailureClass::DeadlineExceeded,
                reason.clone(),
                usage.clone(),
            ))
        }
    };
    Ok(exit)
}

fn capability_output_outcome(
    capability: &ExecutionCapability,
    output: moa_core::types::tools::SecuredToolOutput,
    usage: ExecutionUsage,
) -> ExecutionTaskOutcome {
    if output.is_error() {
        if capability.idempotency_class == IdempotencyClass::Idempotent {
            return failed_task_outcome(
                ExecutionFailureClass::Retryable,
                output.safe_output.to_text(),
                usage,
            );
        }
        if capability.action_class != moa_core::types::action_policy::ActionClass::Read {
            return ExecutionTaskOutcome {
                schema_version: 1,
                usage,
                result: ExecutionTaskResult::UnknownOutcome {
                    message: format!(
                        "non-idempotent side effect returned an error after possible commit: {}",
                        output.safe_output.to_text()
                    ),
                },
            };
        }
        return failed_task_outcome(
            ExecutionFailureClass::Terminal,
            output.safe_output.to_text(),
            usage,
        );
    }
    let value = output
        .safe_output
        .structured_payload()
        .cloned()
        .unwrap_or_else(|| Value::String(output.safe_output.to_text()));
    if let Err(error) = validate_instance(
        &capability.output_schema,
        &value,
        "execution_task.capability_output",
    ) {
        if capability.action_class == moa_core::types::action_policy::ActionClass::Read {
            failed_task_outcome(
                ExecutionFailureClass::InvalidOutput,
                error.to_string(),
                usage,
            )
        } else {
            ExecutionTaskOutcome {
                schema_version: 1,
                usage,
                result: ExecutionTaskResult::UnknownOutcome {
                    message: format!(
                        "side effect returned invalid output after possible commit: {error}"
                    ),
                },
            }
        }
    } else {
        completed_task_outcome(value, usage)
    }
}

async fn execute_agent_turn(
    workflow: &ExecutionTaskAttemptImpl,
    ctx: &WorkflowContext<'_>,
    request: &ExecutionTaskAttemptRequest,
    started: &TaskAttemptRecord,
    spec: AgentTaskSpec<'_>,
    continuation: Option<&TaskAttemptContinuation>,
) -> Result<ActiveTaskAttemptExit, HandlerError> {
    let AgentTaskSpec {
        instructions,
        skill_refs,
        capability_refs,
        max_turns,
    } = spec;
    if max_turns == 0 {
        return Ok(ActiveTaskAttemptExit::Outcome(failed_task_outcome(
            ExecutionFailureClass::InvalidInput,
            "agent max_turns must be positive".to_string(),
            started.task.actual.clone(),
        )));
    }
    let mut capabilities = BTreeMap::<String, &ExecutionCapability>::new();
    for reference in capability_refs {
        let capability = find_capability(&started.run, reference)?;
        let tool_name = capability_tool_name(capability)?;
        if capabilities.insert(tool_name.clone(), capability).is_some() {
            return Err(TerminalError::new(format!(
                "task-local agent has ambiguous capability tool `{tool_name}`"
            ))
            .into());
        }
    }
    let circuit_owner = SecurityCircuitOwner::ExecutionTask {
        run_uid: started.run.run_uid,
        task_uid: started.task.task_id.as_uuid(),
        generation: started.task.generation,
    };
    let (
        mut messages,
        mut next_turn,
        mut usage,
        mut security_circuit,
        mut disabled_capabilities,
        mut pending_review,
        mut pending_tool_calls,
        mut pending_external,
    ) = match continuation {
        Some(TaskAttemptContinuation {
            state:
                TaskAttemptContinuationState::Agent {
                    messages,
                    next_turn,
                    usage,
                    security_circuit,
                    disabled_capabilities,
                    pending_review,
                    pending_tool_calls,
                    pending_external,
                },
            ..
        }) => (
            messages.clone(),
            *next_turn,
            usage.clone(),
            security_circuit.clone(),
            disabled_capabilities.clone(),
            pending_review.as_deref().cloned(),
            pending_tool_calls.clone(),
            pending_external.clone(),
        ),
        Some(_) => {
            return Err(TerminalError::new(
                "task-local agent received an incompatible continuation",
            )
            .into());
        }
        None => {
            let skills = load_pinned_skills(workflow, ctx, started, skill_refs).await?;
            let mut circuit = SecurityCircuitState::default();
            circuit.adopt_owner(&circuit_owner);
            (
                vec![
                    ContextMessage::system(agent_system_prompt(instructions, &skills)),
                    ContextMessage::user(
                        json!({
                            "resolved_input": started.task.input,
                            "resume_inputs": started.task.resume_input_history,
                        })
                        .to_string(),
                    ),
                ],
                0,
                started.task.actual.clone(),
                circuit,
                BTreeMap::new(),
                None,
                Vec::new(),
                None,
            )
        }
    };
    security_circuit.adopt_owner(&circuit_owner);

    if let Some(external) = pending_external.take() {
        if let Some(external_job_uid) = external.external_job_uid {
            let resolution = continuation
                .and_then(|continuation| continuation.external_job_resolution.as_ref())
                .ok_or_else(|| {
                    TerminalError::new("agent external continuation has no terminal resolution")
                })?;
            let tool_use_id = external
                .invocation
                .id
                .clone()
                .unwrap_or_else(|| format!("external-job-{external_job_uid}"));
            match resolution {
                AsyncToolJobTerminalOutcome::Completed { output } => {
                    messages.push(ContextMessage::tool_result(
                        tool_use_id,
                        output.to_string(),
                        None,
                    ));
                }
                AsyncToolJobTerminalOutcome::Failed { error } => {
                    messages.push(ContextMessage::tool_result(
                        tool_use_id,
                        format!("external tool failed: {error}"),
                        None,
                    ));
                }
                AsyncToolJobTerminalOutcome::Cancelled => {
                    messages.push(ContextMessage::tool_result(
                        tool_use_id,
                        "external tool was cancelled",
                        None,
                    ));
                }
                AsyncToolJobTerminalOutcome::UnknownOutcome { error } => {
                    return Ok(ActiveTaskAttemptExit::Outcome(ExecutionTaskOutcome {
                        schema_version: 1,
                        usage,
                        result: ExecutionTaskResult::UnknownOutcome {
                            message: format!("external agent effect outcome is unknown: {error}"),
                        },
                    }));
                }
            }
        } else {
            // Provider start recovery proved NotStarted and re-admitted the exact continuation.
            // Reinsert the original model invocation so its stable tool id/idempotency key is
            // dispatched again without asking the model or repeating prior tool effects.
            pending_tool_calls.insert(0, external.invocation);
        }
    }

    if let Some(reviewed) = pending_review.take() {
        let resolution = continuation
            .and_then(|continuation| continuation.review_resolution.as_ref())
            .ok_or_else(|| {
                TerminalError::new("agent review continuation has no durable resolution")
            })?;
        match resolution {
            moa_execution::wire::ExecutionActionReviewResolution::Completed { tool_output } => {
                let output = serde_json::from_value::<moa_core::types::tools::SecuredToolOutput>(
                    tool_output.clone(),
                )
                .map_err(|error| {
                    TerminalError::new(format!("decode reviewed agent capability output: {error}"))
                })?;
                append_agent_tool_output(&mut messages, &reviewed.invocation, &output);
                usage.retrieved_bytes = usage
                    .retrieved_bytes
                    .saturating_add(serialized_len(&output.safe_output.structured_payload()));
            }
            moa_execution::wire::ExecutionActionReviewResolution::ExternalJob {
                external_job_uid,
                ..
            } => {
                return Ok(ActiveTaskAttemptExit::ExternalJob {
                    external_job_uid: *external_job_uid,
                    continuation: Some(agent_continuation(
                        messages,
                        next_turn,
                        usage,
                        security_circuit,
                        disabled_capabilities,
                        AgentPending {
                            review: None,
                            tool_calls: pending_tool_calls,
                            external: Some(PendingExternalToolInvocation {
                                external_job_uid: None,
                                invocation: reviewed.invocation,
                                effect_idempotency: reviewed.effect_idempotency,
                            }),
                        },
                    )),
                });
            }
            moa_execution::wire::ExecutionActionReviewResolution::Failed { class, message } => {
                return Ok(ActiveTaskAttemptExit::Outcome(failed_task_outcome(
                    class.clone(),
                    message.clone(),
                    usage,
                )));
            }
            moa_execution::wire::ExecutionActionReviewResolution::UnknownOutcome { message } => {
                return Ok(ActiveTaskAttemptExit::Outcome(ExecutionTaskOutcome {
                    schema_version: 1,
                    usage,
                    result: ExecutionTaskResult::UnknownOutcome {
                        message: message.clone(),
                    },
                }));
            }
            moa_execution::wire::ExecutionActionReviewResolution::NotDispatched { reason } => {
                return Ok(ActiveTaskAttemptExit::Outcome(failed_task_outcome(
                    ExecutionFailureClass::Terminal,
                    execution_dispatch_rejection_message(*reason),
                    usage,
                )));
            }
            moa_execution::wire::ExecutionActionReviewResolution::Denied { reason } => {
                return Ok(ActiveTaskAttemptExit::Outcome(failed_task_outcome(
                    ExecutionFailureClass::AuthorizationDenied,
                    reason.clone(),
                    usage,
                )));
            }
            moa_execution::wire::ExecutionActionReviewResolution::TimedOut { reason } => {
                return Ok(ActiveTaskAttemptExit::Outcome(failed_task_outcome(
                    ExecutionFailureClass::DeadlineExceeded,
                    reason.clone(),
                    usage,
                )));
            }
        }
    }

    if pending_tool_calls.is_empty() {
        if next_turn >= max_turns {
            return Ok(ActiveTaskAttemptExit::Outcome(failed_task_outcome(
                ExecutionFailureClass::Terminal,
                format!("task-local agent exhausted max_turns={max_turns}"),
                usage,
            )));
        }
        let mut completion = moa_core::types::completion::CompletionRequest {
            model: None,
            messages: messages.clone(),
            tools: capabilities
                .iter()
                .filter(|(name, _)| !disabled_capabilities.contains_key(*name))
                .map(|(name, capability)| agent_tool_schema(name, capability))
                .collect(),
            max_output_tokens: None,
            temperature: None,
            response_format: None,
            native_web_search: Default::default(),
            metadata: std::collections::HashMap::new(),
        };
        let owner = LLMCompletionOwner::execution_task_attempt(request.dispatch_uid);
        attach_completion_owner(&mut completion, &owner);
        let response = crate::restate_identity::replay_safe_request(
            ctx.service_client::<LLMGatewayClient>()
                .complete_bounded(Json::from(BoundedCompletionRequest {
                    request: completion,
                    budget: ResourceBudget::until(request.attempt_deadline_at),
                }))
                .idempotency_key(completion_idempotency_key(
                    ctx.invocation_id(),
                    LLMCompletionAction::ExecutionTaskModel {
                        generation: started.task.generation,
                        turn: next_turn,
                    },
                )),
        )
        .call()
        .await?
        .into_inner();
        record_attempt_heartbeat(workflow, ctx, request, AttemptHeartbeat::ModelTurn).await?;
        usage.tokens = usage
            .tokens
            .saturating_add(response.usage.total_input_tokens() as u64)
            .saturating_add(response.usage.output_tokens as u64);
        usage.cost_microusd = usage.cost_microusd.saturating_add(
            moa_providers::pricing_for_model(response.model.as_str())
                .map(|pricing| pricing.cost_micros(&response.usage))
                .unwrap_or_default(),
        );
        let tool_calls = response
            .content
            .iter()
            .filter_map(|content| match content {
                CompletionContent::ToolCall(call) => Some(call.invocation.clone()),
                CompletionContent::Text(_) | CompletionContent::ProviderToolResult { .. } => None,
            })
            .collect::<Vec<_>>();
        if tool_calls.is_empty() {
            let outcome = moa_execution::state::parse_agent_task_outcome(&response.text, usage);
            if matches!(outcome.result, ExecutionTaskResult::NeedsInput { .. }) {
                messages.push(ContextMessage::assistant_with_thought_signature(
                    response.text,
                    response.thought_signature,
                ));
                return Ok(ActiveTaskAttemptExit::InputPending {
                    continuation: agent_continuation(
                        messages,
                        next_turn.saturating_add(1),
                        outcome.usage.clone(),
                        security_circuit,
                        disabled_capabilities,
                        AgentPending {
                            review: None,
                            tool_calls: pending_tool_calls,
                            external: pending_external,
                        },
                    ),
                    outcome,
                });
            }
            return Ok(ActiveTaskAttemptExit::Outcome(outcome));
        }
        for (index, invocation) in tool_calls.iter().cloned().enumerate() {
            messages.push(ContextMessage::assistant_tool_call_with_thought_signature(
                invocation,
                if index == 0 {
                    response.text.clone()
                } else {
                    String::new()
                },
                (index == 0)
                    .then(|| response.thought_signature.clone())
                    .flatten(),
            ));
        }
        pending_tool_calls = tool_calls;
        next_turn = next_turn.saturating_add(1);
    }

    let invocation = pending_tool_calls.remove(0);
    let capability = capabilities.get(&invocation.name).copied().ok_or_else(|| {
        TerminalError::new(format!(
            "agent emitted undeclared capability `{}`",
            invocation.name
        ))
    })?;
    if disabled_capabilities.contains_key(&invocation.name) {
        let tool_use_id = invocation
            .id
            .clone()
            .unwrap_or_else(|| format!("execution-{}-{next_turn}", started.task.task_id));
        messages.push(ContextMessage::tool_result(
            tool_use_id,
            "This tool capability is disabled for this task by the security circuit.",
            None,
        ));
        return Ok(ActiveTaskAttemptExit::Continue {
            continuation: agent_continuation(
                messages,
                next_turn,
                usage,
                security_circuit,
                disabled_capabilities,
                AgentPending {
                    review: None,
                    tool_calls: pending_tool_calls,
                    external: pending_external,
                },
            ),
        });
    }
    let session = load_session(workflow, ctx, &started.run, &started.task).await?;
    let tool_id = ToolCallId(Uuid::new_v5(
        &started.task.task_id.as_uuid(),
        format!(
            "agent-tool:{}:{}:{}",
            started.task.generation,
            next_turn,
            invocation.id.as_deref().unwrap_or(&invocation.name)
        )
        .as_bytes(),
    ));
    let tool_call = ToolCallContent {
        invocation: invocation.clone(),
        provider_metadata: None,
    };
    let allowed_tools = capabilities.keys().cloned().collect::<BTreeSet<_>>();
    let provenance = CapabilityProvenance {
        kind: Some(capability_source_kind(&capability.source).to_string()),
        id: Some(format!(
            "{}@{}",
            capability.reference.name, capability.reference.version
        )),
        step_id: Some(started.task.node_id.clone()),
    };
    if matches!(
        capability.async_mode,
        ToolAsyncMode::MayReturnExternalJob { .. }
    ) {
        let provisional = agent_continuation(
            messages.clone(),
            next_turn,
            usage.clone(),
            security_circuit.clone(),
            disabled_capabilities.clone(),
            AgentPending {
                review: None,
                tool_calls: pending_tool_calls.clone(),
                external: Some(PendingExternalToolInvocation {
                    external_job_uid: None,
                    invocation: invocation.clone(),
                    effect_idempotency: capability.idempotency_class,
                }),
            },
        );
        if !persist_external_start_checkpoint(
            workflow,
            ctx,
            request,
            started,
            TaskAttemptCheckpointKind::AgentContinuation,
            &provisional,
        )
        .await?
        {
            return Ok(ActiveTaskAttemptExit::OwnershipLost);
        }
    }
    let governed = invoke_governed_tool(
        ctx,
        GovernedInvocationRequest {
            session: &session,
            identity: &started.run.admitted_identity,
            session_id: started.run.session_id,
            tool_id,
            tool_call: &tool_call,
            allowed_tools: &allowed_tools,
            expected_tool_contract_revision: Some(&capability.contract_revision),
            active_canary: None,
            trusted_sandbox_manifest: None,
            origin: GovernedInvocationOrigin::ExecutionTask {
                run_uid: started.run.run_uid,
                task_uid: started.task.task_id.as_uuid(),
                generation: started.task.generation,
                attempt_generation: request.attempt_generation,
            },
            capability_provenance: Some(&provenance),
            capability_policy_context: Some(&capability.policy_context),
            resource_budget: ResourceBudget::until(request.attempt_deadline_at),
        },
        &workflow.session_limits,
        workflow.session_store.clone(),
        workflow.channel_adapters.as_ref(),
    )
    .await?;
    record_attempt_heartbeat(workflow, ctx, request, AttemptHeartbeat::ToolCall).await?;
    usage.tool_calls = usage.tool_calls.saturating_add(1);
    match governed {
        GovernedInvocationOutcome::Completed(result)
            if result.disposition == GovernedInvocationDisposition::ReviewPending =>
        {
            let review = result.review.ok_or_else(|| {
                TerminalError::new("review-pending agent result is missing durable review identity")
            })?;
            Ok(ActiveTaskAttemptExit::ReviewPending {
                continuation: agent_continuation(
                    messages,
                    next_turn,
                    usage,
                    security_circuit,
                    disabled_capabilities,
                    AgentPending {
                        review: Some(PendingReviewedToolInvocation {
                            review_uid: review.review_uid,
                            expires_at: review.expires_at,
                            invocation: result.invocation,
                            effect_idempotency: capability.idempotency_class,
                        }),
                        tool_calls: pending_tool_calls,
                        external: pending_external,
                    },
                ),
            })
        }
        GovernedInvocationOutcome::Completed(result) => {
            let output = result.output;
            usage.retrieved_bytes = usage
                .retrieved_bytes
                .saturating_add(serialized_len(&output.safe_output.structured_payload()));
            if !output.assessment.is_safe() {
                moa_security::apply_owner_assessment(
                    &mut security_circuit,
                    moa_security::CircuitTarget {
                        session_id: session.id,
                        owner: &circuit_owner,
                        capability: &output.capability,
                        tool_call_id: tool_id,
                    },
                    &output.assessment,
                )
                .map_err(|_| TerminalError::new("agent security assessment owner mismatch"))?;
                let stage = security_circuit.stage(&circuit_owner, &output.capability);
                if !stage.permits_dispatch() {
                    disabled_capabilities
                        .insert(invocation.name.clone(), output.capability.clone());
                }
                if stage == SecurityCircuitStage::Halted {
                    return Ok(ActiveTaskAttemptExit::Outcome(failed_task_outcome(
                        ExecutionFailureClass::Terminal,
                        "task stopped after unsafe capability output".to_string(),
                        usage,
                    )));
                }
                if stage == SecurityCircuitStage::SuspendedForInput {
                    append_agent_tool_output(&mut messages, &invocation, &output);
                    let outcome = ExecutionTaskOutcome {
                        schema_version: 1,
                        usage: usage.clone(),
                        result: ExecutionTaskResult::NeedsInput {
                            question: "A capability returned potentially unsafe content. Continue?"
                                .to_string(),
                            audience: moa_artifacts::execution_plan::InputAudience::User,
                        },
                    };
                    return Ok(ActiveTaskAttemptExit::InputPending {
                        outcome,
                        continuation: agent_continuation(
                            messages,
                            next_turn,
                            usage,
                            security_circuit,
                            disabled_capabilities,
                            AgentPending {
                                review: None,
                                tool_calls: pending_tool_calls,
                                external: pending_external,
                            },
                        ),
                    });
                }
            }
            append_agent_tool_output(&mut messages, &invocation, &output);
            Ok(ActiveTaskAttemptExit::Continue {
                continuation: agent_continuation(
                    messages,
                    next_turn,
                    usage,
                    security_circuit,
                    disabled_capabilities,
                    AgentPending {
                        review: None,
                        tool_calls: pending_tool_calls,
                        external: pending_external,
                    },
                ),
            })
        }
        GovernedInvocationOutcome::ExternalJob {
            external_job_uid, ..
        } => Ok(ActiveTaskAttemptExit::ExternalJob {
            external_job_uid,
            continuation: Some(agent_continuation(
                messages,
                next_turn,
                usage,
                security_circuit,
                disabled_capabilities,
                AgentPending {
                    review: None,
                    tool_calls: pending_tool_calls,
                    external: Some(PendingExternalToolInvocation {
                        external_job_uid: None,
                        invocation,
                        effect_idempotency: capability.idempotency_class,
                    }),
                },
            )),
        }),
        GovernedInvocationOutcome::UnknownOutcome { message, .. } => {
            Ok(ActiveTaskAttemptExit::Outcome(ExecutionTaskOutcome {
                schema_version: 1,
                usage,
                result: ExecutionTaskResult::UnknownOutcome { message },
            }))
        }
        GovernedInvocationOutcome::NotDispatched { reason, .. } => {
            Ok(ActiveTaskAttemptExit::Outcome(failed_task_outcome(
                ExecutionFailureClass::Terminal,
                execution_dispatch_rejection_message(reason),
                usage,
            )))
        }
        GovernedInvocationOutcome::Delegation { .. } => Err(TerminalError::new(
            "execution task agents cannot invoke delegation capabilities",
        )
        .into()),
    }
}

fn agent_continuation(
    messages: Vec<ContextMessage>,
    next_turn: u32,
    usage: ExecutionUsage,
    security_circuit: SecurityCircuitState,
    disabled_capabilities: BTreeMap<String, ToolCapabilityId>,
    pending: AgentPending,
) -> TaskAttemptContinuation {
    TaskAttemptContinuation {
        schema_version: TASK_ATTEMPT_CONTINUATION_SCHEMA_VERSION,
        state: TaskAttemptContinuationState::Agent {
            messages,
            next_turn,
            usage,
            security_circuit,
            disabled_capabilities,
            pending_review: pending.review.map(Box::new),
            pending_tool_calls: pending.tool_calls,
            pending_external: pending.external,
        },
        review_resolution: None,
        external_job_resolution: None,
        workspace_release_receipt_id: None,
    }
}

fn agent_tool_schema(name: &str, capability: &ExecutionCapability) -> Value {
    json!({
        "name": name,
        "description": capability.description,
        "input_schema": capability.input_schema,
    })
}

fn append_agent_tool_output(
    messages: &mut Vec<ContextMessage>,
    invocation: &ToolInvocation,
    output: &moa_core::types::tools::SecuredToolOutput,
) {
    let tool_use_id = invocation.id.clone().unwrap_or_else(|| {
        Uuid::new_v5(
            &Uuid::NAMESPACE_OID,
            format!("{}:{}", invocation.name, invocation.input).as_bytes(),
        )
        .to_string()
    });
    messages.push(ContextMessage::tool_result(
        tool_use_id,
        output.safe_output.to_text(),
        Some(output.safe_output.content.clone()),
    ));
}

async fn load_session(
    workflow: &ExecutionTaskAttemptImpl,
    ctx: &WorkflowContext<'_>,
    run: &moa_execution::repository::ExecutionRunRecord,
    task: &moa_execution::repository::ExecutionTaskRecord,
) -> Result<moa_core::types::session::SessionMeta, HandlerError> {
    let store = workflow.session_store.clone();
    let session_id = run.session_id;
    Ok(ctx
        .run(|| async move {
            store
                .get_session(session_id)
                .await
                .map(Json::from)
                .map_err(moa_error_to_handler_error)
        })
        .name(format!(
            "task_attempt_load_session:{}:{}",
            task.generation, task.attempt_generation
        ))
        .await?
        .into_inner())
}

async fn load_pinned_skills(
    workflow: &ExecutionTaskAttemptImpl,
    ctx: &WorkflowContext<'_>,
    started: &TaskAttemptRecord,
    skill_refs: &[moa_artifacts::reference::ArtifactRef],
) -> Result<Vec<String>, HandlerError> {
    let mut markdown = Vec::with_capacity(skill_refs.len());
    let scope = started.run.contact_id.map_or(
        ActionRuleScope::Tenant {
            tenant_id: started.run.tenant_id,
        },
        |contact_id| ActionRuleScope::Contact {
            tenant_id: started.run.tenant_id,
            contact_id,
        },
    );
    for (index, skill_ref) in skill_refs.iter().enumerate() {
        if !started.run.authorization.skill_refs.contains(skill_ref) {
            return Err(TerminalError::new(
                "task requested a skill outside its authorization envelope",
            )
            .into());
        }
        let pinned = started
            .run
            .pinned_instruction_skills
            .iter()
            .find(|pinned| pinned.skill_ref == *skill_ref)
            .ok_or_else(|| TerminalError::new("task requested an unpinned skill"))?;
        let pool = workflow.pool.clone();
        let revision_uid = pinned.revision_uid;
        let loaded = ctx
            .run(|| async move {
                moa_skills::registry::SkillRegistry::new(pool)
                    .load_skill_markdown(&scope, revision_uid)
                    .await
                    .map(Json::from)
                    .map_err(moa_error_to_handler_error)
            })
            .name(format!("task_attempt_skill:{index}:{revision_uid}"))
            .await?
            .into_inner();
        markdown.push(loaded);
    }
    Ok(markdown)
}

fn find_capability<'a>(
    run: &'a moa_execution::repository::ExecutionRunRecord,
    reference: &CapabilityReference,
) -> Result<&'a ExecutionCapability, HandlerError> {
    if !run.authorization.capability_refs.contains(reference) {
        return Err(TerminalError::new(
            "capability is outside the persisted authorization envelope",
        )
        .into());
    }
    run.catalog
        .capabilities
        .iter()
        .find(|capability| capability.reference == *reference)
        .ok_or_else(|| TerminalError::new("capability is absent from the persisted catalog").into())
}

const fn capability_source_kind(source: &CapabilitySource) -> &'static str {
    match source {
        CapabilitySource::BuiltInTool { .. } => "built_in_tool",
        CapabilitySource::HandTool { .. } => "hand_tool",
        CapabilitySource::McpTool { .. } => "mcp_tool",
        CapabilitySource::ActionArtifact { .. } => "action_artifact",
        CapabilitySource::ConnectorAction { .. } => "connector_action",
        CapabilitySource::InstalledConnectorAction { .. } => "installed_connector_action",
        CapabilitySource::SkillAction { .. } => "skill_action",
        CapabilitySource::SkillCode { .. } => "skill_code",
        CapabilitySource::Memory { .. } => "memory",
        CapabilitySource::Knowledge { .. } => "knowledge",
        CapabilitySource::Model => "model",
    }
}

fn execution_dispatch_rejection_message(reason: ExecutionToolDispatchRejection) -> String {
    let label = match reason {
        ExecutionToolDispatchRejection::OriginNotFound => "origin_not_found",
        ExecutionToolDispatchRejection::StaleGeneration => "stale_generation",
        ExecutionToolDispatchRejection::OperationNotRunning => "operation_not_running",
        ExecutionToolDispatchRejection::RunNotDispatchable => "run_not_dispatchable",
    };
    format!("execution effect was not dispatched: {label}")
}

fn agent_system_prompt(instructions: &str, skills: &[String]) -> String {
    format!(
        "{instructions}\n\nPinned instruction skills:\n{}\n\nReturn only JSON.",
        skills.join("\n\n---\n\n")
    )
}

fn serialized_len<T: Serialize>(value: &T) -> u64 {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use moa_artifacts::execution_plan::ExecutionUsage;
    use moa_core::types::{completion::ToolInvocation, context::ContextMessage};

    use super::*;

    // Pins: a continuation that cannot fit in the bounded durable payload is rejected
    // before persistence so callers must decompose or request a replan.
    #[test]
    fn oversized_agent_continuation_requires_decomposition_offline() {
        let continuation = TaskAttemptContinuation {
            schema_version: TASK_ATTEMPT_CONTINUATION_SCHEMA_VERSION,
            state: TaskAttemptContinuationState::Agent {
                messages: vec![ContextMessage::user(
                    "x".repeat(MAX_TASK_ATTEMPT_CONTINUATION_BYTES),
                )],
                next_turn: 1,
                usage: ExecutionUsage {
                    cost_microusd: 0,
                    tokens: 0,
                    tool_calls: 0,
                    retrieved_bytes: 0,
                },
                security_circuit: SecurityCircuitState::default(),
                disabled_capabilities: std::collections::BTreeMap::new(),
                pending_review: None,
                pending_tool_calls: Vec::new(),
                pending_external: None,
            },
            review_resolution: None,
            external_job_resolution: None,
            workspace_release_receipt_id: None,
        };

        let error = continuation
            .to_bounded_json()
            .expect_err("oversized continuation must fail closed");
        assert!(error.contains("must be decomposed or replanned"));
    }

    // Pins: once an asynchronous provider start commits, the durable checkpoint
    // retains the exact model invocation, effect semantics, and MOA job identity;
    // decoding the checkpoint must not reconstruct or resend that effect.
    #[test]
    fn agent_external_continuation_round_trips_exact_effect_owner_offline() {
        let external_job_uid = Uuid::from_u128(41);
        let invocation = ToolInvocation {
            id: Some("provider-call-7".to_string()),
            name: "render_video".to_string(),
            input: json!({"prompt": "durable sunrise"}),
        };
        let mut continuation = agent_continuation(
            vec![ContextMessage::user("render a durable sunrise")],
            3,
            zero_usage(),
            SecurityCircuitState::default(),
            BTreeMap::new(),
            AgentPending {
                review: None,
                tool_calls: Vec::new(),
                external: Some(PendingExternalToolInvocation {
                    external_job_uid: None,
                    invocation: invocation.clone(),
                    effect_idempotency: IdempotencyClass::NonIdempotent,
                }),
            },
        );

        continuation
            .bind_external_job(external_job_uid)
            .expect("fresh external continuation must accept its durable job identity");
        let persisted = continuation
            .to_bounded_json()
            .expect("exact continuation must fit the durable bound");
        let decoded: TaskAttemptContinuation =
            serde_json::from_value(persisted).expect("persisted continuation must decode");

        let TaskAttemptContinuationState::Agent {
            pending_external: Some(pending),
            next_turn,
            ..
        } = decoded.state
        else {
            panic!("external continuation lost its exact pending effect");
        };
        assert_eq!(next_turn, 3);
        assert_eq!(pending.external_job_uid, Some(external_job_uid));
        assert_eq!(pending.invocation, invocation);
        assert_eq!(pending.effect_idempotency, IdempotencyClass::NonIdempotent);
    }

    // Pins: a storage-only review checkpoint retains the exact reviewed
    // invocation and expiry across serialization, so a resumed attempt consumes
    // the decision without regenerating the provider effect.
    #[test]
    fn agent_review_continuation_round_trips_exact_effect_fence_offline() {
        let review_uid = Uuid::from_u128(51);
        let expires_at = Utc
            .with_ymd_and_hms(2030, 5, 6, 7, 8, 9)
            .single()
            .expect("fixed review expiry");
        let invocation = ToolInvocation {
            id: Some("reviewed-call-2".to_string()),
            name: "publish_release".to_string(),
            input: json!({"version": "2.0.0"}),
        };
        let continuation = agent_continuation(
            vec![ContextMessage::user("publish only after review")],
            2,
            zero_usage(),
            SecurityCircuitState::default(),
            BTreeMap::new(),
            AgentPending {
                review: Some(PendingReviewedToolInvocation {
                    review_uid,
                    expires_at,
                    invocation: invocation.clone(),
                    effect_idempotency: IdempotencyClass::NonIdempotent,
                }),
                tool_calls: Vec::new(),
                external: None,
            },
        );

        let decoded: TaskAttemptContinuation = serde_json::from_value(
            continuation
                .to_bounded_json()
                .expect("review continuation must fit the durable bound"),
        )
        .expect("persisted review continuation must decode");
        assert_eq!(decoded.pending_review_uid(), Some(review_uid));
        let TaskAttemptContinuationState::Agent {
            pending_review: Some(pending),
            ..
        } = decoded.state
        else {
            panic!("review continuation lost its exact pending effect");
        };
        assert_eq!(pending.expires_at, expires_at);
        assert_eq!(pending.invocation, invocation);
    }

    // Pins: an input boundary keeps the already-completed model turn and circuit
    // state in the bounded checkpoint; resumption starts at the following turn
    // instead of calling the model again for the same prompt.
    #[test]
    fn agent_input_continuation_round_trips_next_turn_and_messages_offline() {
        let continuation = agent_continuation(
            vec![
                ContextMessage::user("inspect the unsafe payload"),
                ContextMessage::assistant("May I continue with the unsafe payload?"),
            ],
            4,
            ExecutionUsage {
                cost_microusd: 17,
                tokens: 23,
                tool_calls: 2,
                retrieved_bytes: 31,
            },
            SecurityCircuitState::default(),
            BTreeMap::new(),
            AgentPending {
                review: None,
                tool_calls: Vec::new(),
                external: None,
            },
        );

        let decoded: TaskAttemptContinuation = serde_json::from_value(
            continuation
                .to_bounded_json()
                .expect("input continuation must fit the durable bound"),
        )
        .expect("persisted input continuation must decode");
        let TaskAttemptContinuationState::Agent {
            messages,
            next_turn,
            usage,
            ..
        } = decoded.state
        else {
            panic!("input continuation changed state kind");
        };
        assert_eq!(next_turn, 4);
        assert_eq!(messages.len(), 2);
        assert_eq!(usage.tokens, 23);
        assert_eq!(usage.tool_calls, 2);
    }

    // Pins: provider recovery may prove that a reserved start never happened;
    // the current checkpoint must retain the exact invocation with no job UID so
    // the successor attempt can replay that call without repeating the model turn.
    #[test]
    fn provisional_agent_external_start_round_trips_without_a_job_uid_offline() {
        let invocation = ToolInvocation {
            id: Some("stable-provider-call".to_string()),
            name: "render_video".to_string(),
            input: json!({"prompt": "recover this exact effect"}),
        };
        let continuation = agent_continuation(
            vec![ContextMessage::user("render once")],
            2,
            zero_usage(),
            SecurityCircuitState::default(),
            BTreeMap::new(),
            AgentPending {
                review: None,
                tool_calls: Vec::new(),
                external: Some(PendingExternalToolInvocation {
                    external_job_uid: None,
                    invocation: invocation.clone(),
                    effect_idempotency: IdempotencyClass::NonIdempotent,
                }),
            },
        );

        let decoded: TaskAttemptContinuation = serde_json::from_value(
            continuation
                .to_bounded_json()
                .expect("provisional continuation must fit"),
        )
        .expect("provisional continuation must decode");
        let TaskAttemptContinuationState::Agent {
            pending_external: Some(pending),
            ..
        } = decoded.state
        else {
            panic!("provisional external start lost its pending invocation");
        };
        assert_eq!(pending.external_job_uid, None);
        assert_eq!(pending.invocation, invocation);
    }

    // Pins: a direct async capability resumes with the same stable tool-call ID
    // after a NotStarted recovery instead of creating a second provider identity.
    #[test]
    fn direct_external_start_checkpoint_round_trips_stable_tool_id_offline() {
        let tool_id = ToolCallId(Uuid::from_u128(77));
        let continuation = TaskAttemptContinuation {
            schema_version: TASK_ATTEMPT_CONTINUATION_SCHEMA_VERSION,
            state: TaskAttemptContinuationState::CapabilityExternalStart {
                tool_id,
                usage: zero_usage(),
            },
            review_resolution: None,
            external_job_resolution: None,
            workspace_release_receipt_id: None,
        };

        let decoded: TaskAttemptContinuation = serde_json::from_value(
            continuation
                .to_bounded_json()
                .expect("direct provisional continuation must fit"),
        )
        .expect("direct provisional continuation must decode");
        assert!(matches!(
            decoded.state,
            TaskAttemptContinuationState::CapabilityExternalStart {
                tool_id: decoded_tool_id,
                ..
            } if decoded_tool_id == tool_id
        ));
    }

    const fn zero_usage() -> ExecutionUsage {
        ExecutionUsage {
            cost_microusd: 0,
            tokens: 0,
            tool_calls: 0,
            retrieved_bytes: 0,
        }
    }
}

//! Direct governed-capability execution for one active task attempt.

use std::collections::BTreeSet;

use moa_artifacts::execution_plan::{
    CapabilityReference, ExecutionFailureClass, ExecutionTaskOutcome, ExecutionTaskResult,
    ExecutionUsage,
};
use moa_core::types::{
    action_policy::CapabilityProvenance,
    completion::{ToolCallContent, ToolInvocation},
    identifiers::ToolCallId,
    resource::ResourceBudget,
    tools::{IdempotencyClass, ToolAsyncMode},
};
use moa_execution::{
    capability::ExecutionCapability,
    repository::task::{TaskAttemptCheckpointKind, TaskAttemptRecord},
    schema::validate_instance,
    state::{completed_task_outcome, failed_task_outcome},
    wire::ExecutionTaskAttemptRequest,
};
use restate_sdk::prelude::*;
use serde_json::Value;
use uuid::Uuid;

use crate::{
    tool_invocation::governed::{
        GovernedInvocationDisposition, GovernedInvocationOrigin, GovernedInvocationOutcome,
        GovernedInvocationRequest, invoke_governed_tool,
    },
    workflows::execution_task_attempt::{
        ExecutionTaskAttemptImpl, capability_tool_name,
        continuation::{
            PendingReviewedToolInvocation, TASK_ATTEMPT_CONTINUATION_SCHEMA_VERSION,
            TaskAttemptContinuation, TaskAttemptContinuationState,
        },
    },
};

use super::{
    ActiveTaskAttemptExit, capability_source_kind, execution_dispatch_rejection_message,
    find_capability,
    heartbeat::{AttemptHeartbeat, begin_capability_dispatch, record_attempt_heartbeat},
    load_session, persist_external_start_checkpoint, serialized_len,
};

/// Executes one direct governed capability without waiting on a future event.
pub(super) async fn execute_direct_capability(
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
    if !begin_capability_dispatch(workflow, ctx, request, capability, &tool_call).await? {
        return Ok(ActiveTaskAttemptExit::OwnershipLost);
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
    if !record_attempt_heartbeat(workflow, ctx, request, AttemptHeartbeat::ToolCall).await? {
        return Ok(ActiveTaskAttemptExit::OwnershipLost);
    }
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

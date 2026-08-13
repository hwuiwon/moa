//! Typed exits and shared mechanics for one bounded active task slice.

mod agent;
mod capability;
mod heartbeat;

use moa_artifacts::execution_plan::{
    CapabilityReference, ExecutionFailureClass, ExecutionTaskOutcome,
};
use moa_core::traits::SessionStore as _;
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
use serde::Serialize;
use uuid::Uuid;

use crate::workflows::{
    durable_utc_now,
    errors::moa_error_to_handler_error,
    execution_task_attempt::{
        ExecutionTaskAttemptImpl, continuation::TaskAttemptContinuation, task_attempt_fence,
    },
};

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
            capability::execute_direct_capability(
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
            agent::execute_agent_turn(
                workflow,
                ctx,
                request,
                started,
                agent::AgentTaskSpec {
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
            agent::execute_agent_turn(
                workflow,
                ctx,
                request,
                started,
                agent::AgentTaskSpec {
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

fn serialized_len<T: Serialize>(value: &T) -> u64 {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len() as u64)
        .unwrap_or_default()
}

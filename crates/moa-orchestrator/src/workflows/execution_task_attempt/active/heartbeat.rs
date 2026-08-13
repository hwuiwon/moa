//! Durable heartbeat fencing for active execution-task steps.

use chrono::{DateTime, Utc};
use moa_core::types::{completion::ToolCallContent, tools::ToolAsyncMode};
use moa_execution::{
    capability::ExecutionCapability, repository::task::TaskAttemptProgressOutcome,
    wire::ExecutionTaskAttemptRequest,
};
use restate_sdk::prelude::*;

use crate::workflows::{
    durable_utc_now,
    execution_task_attempt::{ExecutionTaskAttemptImpl, task_attempt_fence},
};

/// Durable step boundary at which an active attempt reports progress.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AttemptHeartbeat {
    /// One model completion is about to start. The bounded gateway budget cannot outlive the
    /// attempt deadline, so the persisted stall window covers that exact call.
    ModelTurnStart,
    /// One model turn returned, so the following tool dispatch starts its own stall window.
    ModelTurn,
    /// One governed tool invocation is about to start with its declared stall bound.
    ToolCallStart { bound: Option<AttemptStepBound> },
    /// One governed tool invocation returned, so sandbox release and continuation persistence
    /// start their own stall window.
    ToolCall,
}

impl AttemptHeartbeat {
    /// Deterministic journal step name for this boundary.
    const fn observation_step(self) -> &'static str {
        match self {
            Self::ModelTurnStart => "task_attempt_model_turn_start_progress_at",
            Self::ModelTurn => "task_attempt_model_turn_progress_at",
            Self::ToolCallStart { .. } => "task_attempt_tool_call_start_progress_at",
            Self::ToolCall => "task_attempt_tool_call_progress_at",
        }
    }

    /// Deterministic journal step name for the persisted heartbeat.
    const fn write_step(self) -> &'static str {
        match self {
            Self::ModelTurnStart => "record_task_attempt_model_turn_start_progress",
            Self::ModelTurn => "record_task_attempt_model_turn_progress",
            Self::ToolCallStart { .. } => "record_task_attempt_tool_call_start_progress",
            Self::ToolCall => "record_task_attempt_tool_call_progress",
        }
    }

    /// Upper bound of the step this boundary opens, when that step declares one.
    ///
    /// Post-return boundaries clear the bound back to the configured heartbeat floor.
    fn step_bound_seconds(
        self,
        request: &ExecutionTaskAttemptRequest,
        observed_at: DateTime<Utc>,
    ) -> Option<u32> {
        match self {
            Self::ModelTurnStart => Some(AttemptStepBound::UntilAttemptDeadline)
                .and_then(|bound| bound.seconds(request, observed_at)),
            Self::ToolCallStart { bound } => {
                bound.and_then(|bound| bound.seconds(request, observed_at))
            }
            Self::ModelTurn | Self::ToolCall => None,
        }
    }
}

/// Returns the bound to record for the capability step this attempt is about to dispatch.
///
/// An external provider start remains bounded by the attempt deadline so its recovery trigger,
/// rather than the task watchdog, retains authority over an ambiguous start.
fn capability_step_bound(
    requires_sandbox: bool,
    async_mode: &ToolAsyncMode,
    tool_call: &ToolCallContent,
) -> Option<AttemptStepBound> {
    if requires_sandbox || matches!(async_mode, ToolAsyncMode::MayReturnExternalJob { .. }) {
        return Some(AttemptStepBound::UntilAttemptDeadline);
    }
    moa_hands::tools::bash::declared_tool_step_bound(
        &tool_call.invocation.name,
        &tool_call.invocation.input,
    )
    .and_then(|bound| u32::try_from(bound.as_secs()).ok())
    .map(AttemptStepBound::Declared)
}

/// Upper bound a dispatching step declares for itself.
///
/// `UntilAttemptDeadline` is resolved against the journaled heartbeat instant rather than a
/// fresh clock read, because a workflow that reads the wall clock outside the journal
/// produces a different value on replay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AttemptStepBound {
    /// The step named its own ceiling in seconds.
    Declared(u32),
    /// The step runs until the attempt deadline and must not be cut short before it.
    UntilAttemptDeadline,
}

impl AttemptStepBound {
    /// Resolves this bound to seconds against one journaled observation instant.
    fn seconds(
        self,
        request: &ExecutionTaskAttemptRequest,
        observed_at: DateTime<Utc>,
    ) -> Option<u32> {
        match self {
            Self::Declared(seconds) => Some(seconds),
            // Rounded up so a sub-second remainder still outlasts the deadline it covers.
            Self::UntilAttemptDeadline => u32::try_from(
                request
                    .attempt_deadline_at
                    .signed_duration_since(observed_at)
                    .num_seconds()
                    .saturating_add(1),
            )
            .ok()
            .filter(|seconds| *seconds > 0),
        }
    }
}

/// Advances the active attempt's durable progress clock and returns whether it still owns it.
///
/// The exact dispatch fence prevents a parked, superseded, or settled attempt from regaining
/// dispatch authority. The observation time is journaled for replay stability.
pub(super) async fn record_attempt_heartbeat(
    workflow: &ExecutionTaskAttemptImpl,
    ctx: &WorkflowContext<'_>,
    request: &ExecutionTaskAttemptRequest,
    boundary: AttemptHeartbeat,
) -> Result<bool, HandlerError> {
    let observed_at = durable_utc_now(ctx, boundary.observation_step()).await?;
    let repository = workflow.repository.clone();
    let fence = task_attempt_fence(request);
    Ok(ctx
        .run(|| async move {
            repository
                .record_task_attempt_progress(
                    fence,
                    observed_at,
                    boundary.step_bound_seconds(request, observed_at),
                )
                .await
                .map(|outcome| Json::from(attempt_progress_retains_ownership(outcome)))
                .map_err(crate::workflows::errors::execution_error_to_handler_error)
        })
        .name(boundary.write_step())
        .await?
        .into_inner())
}

const fn attempt_progress_retains_ownership(outcome: TaskAttemptProgressOutcome) -> bool {
    matches!(
        outcome,
        TaskAttemptProgressOutcome::Applied | TaskAttemptProgressOutcome::Replayed
    )
}

/// Records the exact capability bound and confirms ownership before provider dispatch.
pub(super) async fn begin_capability_dispatch(
    workflow: &ExecutionTaskAttemptImpl,
    ctx: &WorkflowContext<'_>,
    request: &ExecutionTaskAttemptRequest,
    capability: &ExecutionCapability,
    tool_call: &ToolCallContent,
) -> Result<bool, HandlerError> {
    record_attempt_heartbeat(
        workflow,
        ctx,
        request,
        AttemptHeartbeat::ToolCallStart {
            bound: capability_step_bound(
                capability.requires_sandbox,
                &capability.async_mode,
                tool_call,
            ),
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Utc};
    use moa_core::types::{completion::ToolInvocation, identifiers::TenantId};
    use moa_execution::state::ExecutionTaskId;
    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    // Pins: every pre-provider heartbeat is an ownership check, not telemetry. A stale,
    // absent, or non-running attempt must stop before model or tool dispatch, while exact
    // replay of an already-journaled heartbeat retains authority.
    #[test]
    fn heartbeat_verdict_stops_dispatch_after_ownership_loss_offline() {
        assert!(attempt_progress_retains_ownership(
            TaskAttemptProgressOutcome::Applied
        ));
        assert!(attempt_progress_retains_ownership(
            TaskAttemptProgressOutcome::Replayed
        ));
        for lost in [
            TaskAttemptProgressOutcome::NotFound,
            TaskAttemptProgressOutcome::Stale,
            TaskAttemptProgressOutcome::InvalidState,
        ] {
            assert!(!attempt_progress_retains_ownership(lost));
        }
    }

    // Pins: a healthy model or tool call whose declared duration exceeds the configured
    // heartbeat floor remains live for that exact step, and the first post-return heartbeat
    // clears the widened bound back to the ordinary orchestration floor.
    #[test]
    fn model_and_tool_steps_are_bounded_before_dispatch_and_cleared_after_return_offline() {
        let observed_at = Utc
            .with_ymd_and_hms(2026, 8, 13, 12, 0, 0)
            .single()
            .expect("fixture timestamp is valid");
        let request = ExecutionTaskAttemptRequest {
            dispatch_uid: Uuid::from_u128(1),
            capacity_reservation_uid: Uuid::from_u128(2),
            watchdog_trigger_uid: Uuid::from_u128(3),
            watchdog_dispatch_uid: Uuid::from_u128(4),
            run_uid: Uuid::from_u128(5),
            task_id: ExecutionTaskId::from_uuid(Uuid::from_u128(6)),
            controller_generation: 7,
            attempt_generation: 8,
            attempt_deadline_at: observed_at + Duration::seconds(121),
            tenant_id: TenantId(Uuid::from_u128(9)),
        };

        assert_eq!(
            AttemptHeartbeat::ModelTurnStart.step_bound_seconds(&request, observed_at),
            Some(122),
        );
        assert_eq!(
            AttemptHeartbeat::ToolCallStart {
                bound: Some(AttemptStepBound::Declared(90)),
            }
            .step_bound_seconds(&request, observed_at),
            Some(90),
        );
        assert_eq!(
            AttemptHeartbeat::ModelTurn.step_bound_seconds(&request, observed_at),
            None,
        );
        assert_eq!(
            AttemptHeartbeat::ToolCall.step_bound_seconds(&request, observed_at),
            None,
        );
    }

    // Pins: sandbox lifecycle work shares the active attempt deadline because provisioning,
    // restore, install, execution, and commit can outlive the command timeout alone. A
    // non-sandbox synchronous call keeps its narrower declared execution bound.
    #[test]
    fn sandbox_capability_uses_attempt_bound_while_non_sandbox_keeps_tool_bound_offline() {
        let tool_call = ToolCallContent {
            invocation: ToolInvocation {
                id: Some("bounded-bash".to_string()),
                name: "bash".to_string(),
                input: json!({"cmd": "sleep 1", "timeout_secs": 90}),
            },
            provider_metadata: None,
        };

        assert_eq!(
            capability_step_bound(true, &ToolAsyncMode::SynchronousOnly, &tool_call),
            Some(AttemptStepBound::UntilAttemptDeadline),
        );
        assert_eq!(
            capability_step_bound(false, &ToolAsyncMode::SynchronousOnly, &tool_call),
            Some(AttemptStepBound::Declared(90)),
        );
        assert_eq!(
            capability_step_bound(
                false,
                &ToolAsyncMode::MayReturnExternalJob {
                    provider: "fixture".to_string(),
                },
                &tool_call,
            ),
            Some(AttemptStepBound::UntilAttemptDeadline),
        );
    }
}

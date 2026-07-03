//! Progress and cancellation state helpers shared by turn workflows.

use moa_core::config::SessionLimitsConfig;
use moa_core::wire::turn::{TurnComplexityClass, TurnPhase, TurnProgress};
use restate_sdk::prelude::*;

use crate::turn::util::meaningful_cancel_reason;
use crate::workflows::turn_progress;
use crate::workflows::turn_responsiveness::{progress_cap, progress_count as capped_count};

/// Shared workflow state keys used by root and worker turn workflows.
pub(crate) struct TurnStateKey;

impl TurnStateKey {
    /// Promise key resolved by shared cancellation handlers.
    pub(crate) const CANCEL_REASON_PROMISE: &'static str = "cancel_reason";
    /// Current workflow lifecycle phase.
    const PHASE: &'static str = "phase";
    /// Selected deterministic turn complexity class.
    const COMPLEXITY_CLASS: &'static str = "complexity_class";
    /// Current model-loop iteration counter.
    const ITERATION: &'static str = "iteration";
    /// Current model-loop cap exposed in progress snapshots.
    const MAX_TURNS: &'static str = "max_turns";
    /// Tool-call attempts exposed in progress snapshots.
    const TOOL_CALLS: &'static str = "tool_calls";
    /// Tool-call cap exposed in progress snapshots.
    const MAX_TOOL_CALLS: &'static str = "max_tool_calls";
}

/// Root-turn-only workflow state keys.
pub(crate) struct RootTurnStateKey;

impl RootTurnStateKey {
    /// Sequence number of the admitted root user message.
    pub(crate) const USER_MESSAGE_SEQUENCE: &'static str = "user_message_sequence";
    /// Cached query-rewrite result from the last context compilation pass.
    pub(crate) const QUERY_REWRITE_CACHE: &'static str = "query_rewrite_cache";
    /// Sequence number of the latest assistant response appended by this root turn.
    pub(crate) const LAST_RESPONSE_SEQUENCE: &'static str = "last_response_sequence";
    /// User-message sequence for which deterministic ready delegation nodes were spawned.
    pub(crate) const AUTO_DELEGATION_SEQUENCE: &'static str = "auto_delegation_sequence";
    /// Worker ids spawned by deterministic auto-delegation for the admitted user message.
    pub(crate) const AUTO_DELEGATION_WORKER_IDS: &'static str = "auto_delegation_worker_ids";
    /// User-message sequence for which auto-delegated worker results were bundled.
    pub(crate) const AUTO_DELEGATION_FAN_IN_SEQUENCE: &'static str =
        "auto_delegation_fan_in_sequence";
    /// Worker id the root fan-in has been waiting on across consecutive stuck cycles.
    pub(crate) const AUTO_DELEGATION_FAN_IN_STUCK_WORKER: &'static str =
        "auto_delegation_fan_in_stuck_worker";
    /// Consecutive fan-in wait cycles spent on the same still-pending worker, used to bound the
    /// wait so one never-terminal worker cannot hang the whole session.
    pub(crate) const AUTO_DELEGATION_FAN_IN_STUCK_COUNT: &'static str =
        "auto_delegation_fan_in_stuck_count";
}

/// Progress cadence derived from session limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProgressCadence {
    /// Delay before the first progress update may be emitted.
    pub(crate) first_delay_ms: u64,
    /// Minimum interval between emitted progress updates.
    pub(crate) interval_ms: u64,
}

/// Returns the configured progress cadence for a turn workflow.
pub(crate) fn cadence_from_limits(limits: &SessionLimitsConfig) -> ProgressCadence {
    ProgressCadence {
        first_delay_ms: limits.progress_first_delay_ms,
        interval_ms: limits.progress_interval_ms,
    }
}

/// Returns the current runtime progress cadence.
pub(crate) fn current_cadence() -> ProgressCadence {
    cadence_from_limits(&crate::OrchestratorCtx::current_config().session_limits)
}

/// Returns whether the phase no longer accepts cancellation changes.
pub(crate) fn is_terminal_phase(phase: &TurnPhase) -> bool {
    matches!(
        phase,
        TurnPhase::Completed | TurnPhase::Cancelled | TurnPhase::Failed
    )
}

/// Stores the current workflow phase.
pub(crate) fn set_phase(ctx: &WorkflowContext<'_>, phase: TurnPhase) {
    ctx.set(TurnStateKey::PHASE, Json::from(phase));
}

/// Initializes shared model-loop progress fields.
pub(crate) fn initialize_loop_progress(
    ctx: &WorkflowContext<'_>,
    complexity_class: TurnComplexityClass,
    max_turns: usize,
    max_tool_calls: usize,
) {
    ctx.set(TurnStateKey::COMPLEXITY_CLASS, Json::from(complexity_class));
    ctx.set(TurnStateKey::ITERATION, Json::from(0_u32));
    ctx.set(TurnStateKey::MAX_TURNS, Json::from(progress_cap(max_turns)));
    ctx.set(TurnStateKey::TOOL_CALLS, Json::from(0_u32));
    ctx.set(
        TurnStateKey::MAX_TOOL_CALLS,
        Json::from(progress_cap(max_tool_calls)),
    );
}

/// Stores the current model-loop iteration using the progress DTO cap.
pub(crate) fn set_iteration(ctx: &WorkflowContext<'_>, turn_number: usize) {
    ctx.set(
        TurnStateKey::ITERATION,
        Json::from(capped_count(turn_number)),
    );
}

/// Stores the current attempted tool-call count using the progress DTO cap.
pub(crate) fn set_tool_calls(ctx: &WorkflowContext<'_>, attempted_tool_calls: usize) {
    ctx.set(
        TurnStateKey::TOOL_CALLS,
        Json::from(capped_count(attempted_tool_calls)),
    );
}

/// Resolves cancellation when the workflow is not already terminal.
pub(crate) async fn request_cancel(
    ctx: &SharedWorkflowContext<'_>,
    reason: String,
) -> Result<(), HandlerError> {
    let phase = ctx
        .get::<Json<TurnPhase>>(TurnStateKey::PHASE)
        .await?
        .map(Json::into_inner)
        .unwrap_or_default();
    if is_terminal_phase(&phase) {
        return Ok(());
    }

    let Some(reason) = meaningful_cancel_reason(Some(reason)) else {
        return Ok(());
    };
    ctx.resolve_promise(TurnStateKey::CANCEL_REASON_PROMISE, reason);
    Ok(())
}

/// Returns a meaningful cancellation reason if one has been requested.
pub(crate) async fn cancel_requested(
    ctx: &WorkflowContext<'_>,
) -> Result<Option<String>, HandlerError> {
    Ok(meaningful_cancel_reason(
        ctx.peek_promise::<String>(TurnStateKey::CANCEL_REASON_PROMISE)
            .await?,
    ))
}

/// Builds the shared progress response for workflow progress handlers.
pub(crate) async fn snapshot(
    ctx: &SharedWorkflowContext<'_>,
) -> Result<Json<TurnProgress>, HandlerError> {
    let phase = ctx
        .get::<Json<TurnPhase>>(TurnStateKey::PHASE)
        .await?
        .map(Json::into_inner)
        .unwrap_or_default();
    let cancel_reason = meaningful_cancel_reason(
        ctx.peek_promise::<String>(TurnStateKey::CANCEL_REASON_PROMISE)
            .await?,
    );
    let complexity_class = ctx
        .get::<Json<TurnComplexityClass>>(TurnStateKey::COMPLEXITY_CLASS)
        .await?
        .map(Json::into_inner)
        .unwrap_or_default();
    let iteration = ctx
        .get::<Json<u32>>(TurnStateKey::ITERATION)
        .await?
        .map(Json::into_inner)
        .unwrap_or_default();
    let max_turns = ctx
        .get::<Json<Option<u32>>>(TurnStateKey::MAX_TURNS)
        .await?
        .map(Json::into_inner)
        .unwrap_or(None);
    let tool_calls = ctx
        .get::<Json<u32>>(TurnStateKey::TOOL_CALLS)
        .await?
        .map(Json::into_inner)
        .unwrap_or_default();
    let max_tool_calls = ctx
        .get::<Json<Option<u32>>>(TurnStateKey::MAX_TOOL_CALLS)
        .await?
        .map(Json::into_inner)
        .unwrap_or(None);
    let progress = turn_progress::snapshot(ctx, is_terminal_phase(&phase)).await?;
    Ok(Json::from(TurnProgress {
        turn_id: ctx.key().to_string(),
        phase,
        complexity_class,
        iteration,
        max_turns,
        tool_calls,
        max_tool_calls,
        elapsed_ms: progress.elapsed_ms,
        last_progress_summary: progress.last_summary,
        cancel_requested: cancel_reason.is_some(),
        cancel_reason,
    }))
}

#[cfg(test)]
mod tests {
    use moa_core::config::SessionLimitsConfig;
    use moa_core::wire::turn::TurnPhase;

    use super::{cadence_from_limits, is_terminal_phase};

    #[test]
    fn terminal_phase_detection_matches_workflow_lifecycle() {
        // Pins: cancellation requests stop mutating completed workflows.
        assert!(!is_terminal_phase(&TurnPhase::Pending));
        assert!(!is_terminal_phase(&TurnPhase::Compiling));
        assert!(!is_terminal_phase(&TurnPhase::Streaming));
        assert!(!is_terminal_phase(&TurnPhase::Tooling));
        assert!(!is_terminal_phase(&TurnPhase::Persisting));
        assert!(is_terminal_phase(&TurnPhase::Completed));
        assert!(is_terminal_phase(&TurnPhase::Cancelled));
        assert!(is_terminal_phase(&TurnPhase::Failed));
    }

    #[test]
    fn progress_cadence_uses_session_limits_directly() {
        // Pins: workflows expose configured progress cadence without workflow-local constants.
        let limits = SessionLimitsConfig {
            progress_first_delay_ms: 123,
            progress_interval_ms: 456,
            ..SessionLimitsConfig::default()
        };
        let cadence = cadence_from_limits(&limits);
        assert_eq!(cadence.first_delay_ms, 123);
        assert_eq!(cadence.interval_ms, 456);
    }
}

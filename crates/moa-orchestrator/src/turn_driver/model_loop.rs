//! Deterministic model-loop planning helpers for turn workflows.

use moa_core::config::SessionLimitsConfig;
use moa_core::types::execution_planning::{
    ExecutionMode, ExecutionRouteDecision, ExecutionRouteReason,
};

use crate::workflows::turn_responsiveness::{
    ToolBudgetState, effective_tool_cap, effective_turn_cap,
};

/// Inputs for planning one root session turn loop.
#[derive(Clone, Debug)]
pub(crate) struct RootLoopPlanRequest<'a> {
    /// Deterministic route selected for this root turn.
    pub(crate) route: &'a ExecutionRouteDecision,
    /// Caller-supplied turn cap.
    pub(crate) request_max_turns: Option<u32>,
}

/// Inputs for planning one worker turn loop.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WorkerLoopPlanRequest {
    /// Caller-supplied worker turn cap.
    pub(crate) request_max_turns: Option<u32>,
    /// Default worker workflow cap used when the caller omits one.
    pub(crate) default_max_turns: usize,
}

/// Deterministic model-loop plan shared by workflow shells.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TurnLoopPlan {
    /// Selected deterministic execution route.
    pub(crate) route: ExecutionRouteDecision,
    /// Effective model-loop cap.
    pub(crate) max_turns: usize,
    /// Effective tool-call cap.
    pub(crate) max_tool_calls: usize,
    loop_detection_threshold: u32,
}

impl TurnLoopPlan {
    /// Creates a fresh per-turn tool budget state for this plan.
    pub(crate) fn tool_budget(&self) -> ToolBudgetState {
        ToolBudgetState::new(self.max_tool_calls, self.loop_detection_threshold)
    }
}

/// Builds a deterministic loop plan for a root session turn.
pub(crate) fn root_loop_plan(
    request: RootLoopPlanRequest<'_>,
    session_limits: &SessionLimitsConfig,
) -> TurnLoopPlan {
    loop_plan(
        request.request_max_turns,
        request.route.clone(),
        session_limits,
    )
}

/// Builds a deterministic loop plan for a delegated worker turn.
pub(crate) fn worker_loop_plan(
    request: WorkerLoopPlanRequest,
    session_limits: &SessionLimitsConfig,
) -> TurnLoopPlan {
    let route = ExecutionRouteDecision::Routed {
        mode: ExecutionMode::Act,
        reason: ExecutionRouteReason::BoundedInteractiveWork,
    };
    let default_cap = request.default_max_turns.min(u32::MAX as usize) as u32;
    let request_or_default_cap = request.request_max_turns.or(Some(default_cap));
    loop_plan(request_or_default_cap, route, session_limits)
}

fn loop_plan(
    request_max_turns: Option<u32>,
    route: ExecutionRouteDecision,
    session_limits: &SessionLimitsConfig,
) -> TurnLoopPlan {
    let max_turns = effective_turn_cap(request_max_turns, &route, session_limits);
    let max_tool_calls = effective_tool_cap(&route, session_limits);
    TurnLoopPlan {
        route,
        max_turns,
        max_tool_calls,
        loop_detection_threshold: session_limits.loop_detection_threshold,
    }
}

#[cfg(test)]
mod tests {
    use moa_core::config::SessionLimitsConfig;
    use moa_core::types::execution_planning::{
        ExecutionMode, ExecutionRouteDecision, ExecutionRouteReason,
    };

    use super::{RootLoopPlanRequest, WorkerLoopPlanRequest, root_loop_plan, worker_loop_plan};

    #[test]
    fn root_vague_turn_uses_clarification_budget() {
        // Pins: root clarification planning stays deterministic outside workflow code.
        let limits = SessionLimitsConfig::default();
        let plan = root_loop_plan(
            RootLoopPlanRequest {
                route: &ExecutionRouteDecision::NeedsInput {
                    reason: ExecutionRouteReason::PreflightInputMissing,
                },
                request_max_turns: None,
            },
            &limits,
        );
        assert!(matches!(
            plan.route,
            ExecutionRouteDecision::NeedsInput { .. }
        ));
        assert_eq!(plan.max_turns, limits.simple_max_turns as usize);
        assert_eq!(plan.max_tool_calls, 0);
    }

    #[test]
    fn worker_plan_is_complex_and_defaults_cap() {
        // Pins: worker workflows keep their bounded default loop cap.
        let limits = SessionLimitsConfig::default();
        let plan = worker_loop_plan(
            WorkerLoopPlanRequest {
                request_max_turns: None,
                default_max_turns: 7,
            },
            &limits,
        );
        assert_eq!(
            plan.route,
            ExecutionRouteDecision::Routed {
                mode: ExecutionMode::Act,
                reason: ExecutionRouteReason::BoundedInteractiveWork,
            }
        );
        assert_eq!(plan.max_turns, 7);
        assert_eq!(plan.max_tool_calls, limits.max_tool_calls as usize);
    }
}

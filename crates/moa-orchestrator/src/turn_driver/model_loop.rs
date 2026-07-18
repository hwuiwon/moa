//! Deterministic model-loop planning helpers for turn workflows.

use moa_core::config::SessionLimitsConfig;
use moa_core::types::execution_planning::{ExecutionRouteDecision, ExecutionStrategy};

use crate::workflows::turn_responsiveness::{
    ModelLoopClass, ToolBudgetState, effective_tool_cap, effective_turn_cap,
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
    /// Selected root execution route, absent for intrinsically Inline workers.
    pub(crate) route: Option<ExecutionRouteDecision>,
    /// Exact model-loop class used to derive caps and tool visibility.
    pub(crate) class: ModelLoopClass,
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
) -> Option<TurnLoopPlan> {
    let class = match request.route {
        ExecutionRouteDecision::Respond { .. } => ModelLoopClass::Respond,
        ExecutionRouteDecision::Execute { reason }
            if reason.strategy() == Some(ExecutionStrategy::Inline) =>
        {
            ModelLoopClass::InlineExecute
        }
        ExecutionRouteDecision::Execute { .. } | ExecutionRouteDecision::NeedsInput { .. } => {
            return None;
        }
    };
    Some(loop_plan(
        request.request_max_turns,
        class,
        Some(request.route.clone()),
        session_limits,
    ))
}

/// Builds a deterministic loop plan for a delegated worker turn.
pub(crate) fn worker_loop_plan(
    request: WorkerLoopPlanRequest,
    session_limits: &SessionLimitsConfig,
) -> TurnLoopPlan {
    let default_cap = request.default_max_turns.min(u32::MAX as usize) as u32;
    let request_or_default_cap = request.request_max_turns.or(Some(default_cap));
    loop_plan(
        request_or_default_cap,
        ModelLoopClass::WorkerInline,
        None,
        session_limits,
    )
}

fn loop_plan(
    request_max_turns: Option<u32>,
    class: ModelLoopClass,
    route: Option<ExecutionRouteDecision>,
    session_limits: &SessionLimitsConfig,
) -> TurnLoopPlan {
    let max_turns = effective_turn_cap(request_max_turns, class, session_limits);
    let max_tool_calls = effective_tool_cap(class, session_limits);
    TurnLoopPlan {
        route,
        class,
        max_turns,
        max_tool_calls,
        loop_detection_threshold: session_limits.loop_detection_threshold,
    }
}

#[cfg(test)]
mod tests {
    use moa_core::config::SessionLimitsConfig;
    use moa_core::types::execution_planning::{ExecutionRouteDecision, ExecutionRouteReason};

    use super::{RootLoopPlanRequest, WorkerLoopPlanRequest, root_loop_plan, worker_loop_plan};

    #[test]
    fn root_respond_and_inline_execute_use_their_bounded_loop_classes() {
        // Pins: only Respond and Execute/Inline construct root model loops, with no tools
        // for Respond and the existing standard work caps for Inline Execute.
        let limits = SessionLimitsConfig::default();
        let respond = root_loop_plan(
            RootLoopPlanRequest {
                route: &ExecutionRouteDecision::Respond {
                    reason: ExecutionRouteReason::SimpleResponse,
                },
                request_max_turns: None,
            },
            &limits,
        )
        .expect("Respond should construct a root loop");
        assert!(matches!(
            respond.route,
            Some(ExecutionRouteDecision::Respond { .. })
        ));
        assert_eq!(respond.max_turns, limits.simple_max_turns as usize);
        assert_eq!(respond.max_tool_calls, 0);

        let inline = root_loop_plan(
            RootLoopPlanRequest {
                route: &ExecutionRouteDecision::Execute {
                    reason: ExecutionRouteReason::BoundedInteractiveWork,
                },
                request_max_turns: None,
            },
            &limits,
        )
        .expect("Execute/Inline should construct a root loop");
        assert!(matches!(
            inline.route,
            Some(ExecutionRouteDecision::Execute { .. })
        ));
        assert_eq!(inline.max_turns, limits.standard_max_turns as usize);
        assert_eq!(inline.max_tool_calls, limits.max_tool_calls as usize);
    }

    #[test]
    fn durable_execute_and_needs_input_have_no_model_loop_plan() {
        // Pins: Durable and NeedsInput branch before loop construction and receive no
        // accidental turn or tool budget.
        let limits = SessionLimitsConfig::default();
        for route in [
            ExecutionRouteDecision::Execute {
                reason: ExecutionRouteReason::ExplicitDurableExecution,
            },
            ExecutionRouteDecision::NeedsInput {
                reason: ExecutionRouteReason::PreflightInputMissing,
                missing_inputs: vec!["target".to_string()],
            },
        ] {
            assert!(
                root_loop_plan(
                    RootLoopPlanRequest {
                        route: &route,
                        request_max_turns: None,
                    },
                    &limits,
                )
                .is_none(),
                "non-loop route must not construct a plan: {route:?}"
            );
        }
    }

    #[test]
    fn worker_plan_is_intrinsically_inline_without_a_root_route() {
        // Pins: worker workflows keep their bounded Inline caps without fabricating a
        // root semantic route or Durable-upgrade authority.
        let limits = SessionLimitsConfig::default();
        let plan = worker_loop_plan(
            WorkerLoopPlanRequest {
                request_max_turns: None,
                default_max_turns: 7,
            },
            &limits,
        );
        assert_eq!(plan.route, None);
        assert_eq!(plan.max_turns, 7);
        assert_eq!(plan.max_tool_calls, limits.max_tool_calls as usize);
    }
}

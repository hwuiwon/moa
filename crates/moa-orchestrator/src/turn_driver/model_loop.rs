//! Deterministic model-loop planning helpers for turn workflows.

use moa_config::SessionLimitsConfig;
use moa_core::types::{
    execution_planning::{ExecutionRouteDecision, ExecutionStrategy},
    resource::ResourceBudget,
};

use crate::workflows::turn_responsiveness::{
    ModelLoopClass, ToolBudgetState, effective_delegation_turn_cap, effective_tool_cap,
    effective_turn_cap,
};

/// Inputs for planning one root session turn loop.
#[derive(Clone, Debug)]
pub(crate) struct RootLoopPlanRequest<'a> {
    /// Deterministic route selected for this root turn.
    pub(crate) route: &'a ExecutionRouteDecision,
    /// Caller-supplied turn cap.
    pub(crate) request_max_turns: Option<u32>,
    /// Downward-only resource slice admitted for this turn.
    pub(crate) resource_budget: ResourceBudget,
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
    /// Effective model-loop cap before any worker delegation.
    pub(crate) max_turns: usize,
    /// Effective model-loop cap once this turn has delegated to a worker. Always
    /// greater than or equal to `max_turns`.
    pub(crate) delegation_max_turns: usize,
    /// Effective tool-call cap.
    pub(crate) max_tool_calls: usize,
    loop_detection_threshold: u32,
}

impl TurnLoopPlan {
    /// Creates a fresh per-turn tool budget state for this plan.
    pub(crate) fn tool_budget(&self) -> ToolBudgetState {
        ToolBudgetState::new(self.max_tool_calls, self.loop_detection_threshold)
    }

    /// Creates a fresh one-way delegation cap escalation for this turn.
    pub(crate) fn turn_cap_escalation(&self) -> TurnCapEscalation {
        TurnCapEscalation::new(self.max_turns, self.delegation_max_turns)
    }

    /// Narrows loop and tool caps to a caller-admitted resource slice.
    fn within(mut self, budget: ResourceBudget) -> Self {
        let Some(remaining) = budget.remaining else {
            return self;
        };
        let model_turns = remaining.turns.min(remaining.model_calls);
        let model_turns = usize::try_from(model_turns).unwrap_or(usize::MAX);
        let tool_calls = usize::try_from(remaining.tool_calls).unwrap_or(usize::MAX);
        self.max_turns = self.max_turns.min(model_turns);
        self.delegation_max_turns = self.delegation_max_turns.min(model_turns);
        self.max_tool_calls = self.max_tool_calls.min(tool_calls);
        self
    }
}

/// One-way escalation of a turn's model-loop cap after it delegates to a worker.
///
/// A turn starts bounded by the base cap. The first successful worker spawn latches
/// the escalation on, so the remainder of that turn is bounded by the higher
/// delegation cap. The escalation never reverses, so the effective cap is monotonic
/// non-decreasing across a turn's model-loop iterations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TurnCapEscalation {
    base_max_turns: usize,
    delegation_max_turns: usize,
    delegated: bool,
}

impl TurnCapEscalation {
    /// Creates a non-delegated escalation bounded by the base cap.
    pub(crate) fn new(base_max_turns: usize, delegation_max_turns: usize) -> Self {
        Self {
            base_max_turns,
            delegation_max_turns,
            delegated: false,
        }
    }

    /// Latches the delegation escalation on, returning whether this call transitioned
    /// it (`true` only on the first delegation of the turn).
    pub(crate) fn record_delegation(&mut self) -> bool {
        let transitioned = !self.delegated;
        self.delegated = true;
        transitioned
    }

    /// Returns the model-loop cap in force for the current iteration.
    pub(crate) fn effective_max_turns(&self) -> usize {
        if self.delegated {
            self.delegation_max_turns
        } else {
            self.base_max_turns
        }
    }
}

/// Builds a deterministic loop plan for a root session turn.
pub(crate) fn root_loop_plan(
    request: RootLoopPlanRequest<'_>,
    session_limits: &SessionLimitsConfig,
) -> Option<TurnLoopPlan> {
    let class = match request.route {
        ExecutionRouteDecision::Respond { .. } => ModelLoopClass::Respond,
        ExecutionRouteDecision::Execute {
            strategy: ExecutionStrategy::Inline,
            ..
        } => ModelLoopClass::InlineExecute,
        ExecutionRouteDecision::Execute { .. } | ExecutionRouteDecision::NeedsInput { .. } => {
            return None;
        }
    };
    Some(
        loop_plan(
            request.request_max_turns,
            class,
            Some(request.route.clone()),
            session_limits,
        )
        .within(request.resource_budget),
    )
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
    let delegation_max_turns =
        effective_delegation_turn_cap(request_max_turns, class, session_limits);
    let max_tool_calls = effective_tool_cap(class, session_limits);
    TurnLoopPlan {
        route,
        class,
        max_turns,
        delegation_max_turns,
        max_tool_calls,
        loop_detection_threshold: session_limits.loop_detection_threshold,
    }
}

#[cfg(test)]
mod tests {
    use moa_config::SessionLimitsConfig;
    use moa_core::types::{
        execution_planning::{ExecutionRouteDecision, ExecutionStrategy},
        resource::{ResourceAmounts, ResourceBudget},
    };

    use super::{
        RootLoopPlanRequest, TurnCapEscalation, WorkerLoopPlanRequest, root_loop_plan,
        worker_loop_plan,
    };

    #[test]
    fn root_respond_and_inline_execute_use_their_bounded_loop_classes() {
        // Pins: only Respond and Execute/Inline construct root model loops, with no tools
        // for Respond and the existing standard work caps for Inline Execute.
        let limits = SessionLimitsConfig::default();
        let respond = root_loop_plan(
            RootLoopPlanRequest {
                route: &ExecutionRouteDecision::Respond {
                    rationale: "The request only needs a response.".to_string(),
                },
                request_max_turns: None,
                resource_budget: ResourceBudget::UNBOUNDED,
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
                    strategy: ExecutionStrategy::Inline,
                    rationale: "The work fits a bounded interactive loop.".to_string(),
                },
                request_max_turns: None,
                resource_budget: ResourceBudget::UNBOUNDED,
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
    fn inline_execute_plan_carries_base_and_delegation_caps() {
        // Pins: a root Inline Execute turn plans both its base loop cap and the higher
        // delegation cap that applies once it spawns a worker.
        let limits = SessionLimitsConfig::default();
        let plan = root_loop_plan(
            RootLoopPlanRequest {
                route: &ExecutionRouteDecision::Execute {
                    strategy: ExecutionStrategy::Inline,
                    rationale: "The work fits a bounded interactive loop.".to_string(),
                },
                request_max_turns: None,
                resource_budget: ResourceBudget::UNBOUNDED,
            },
            &limits,
        )
        .expect("Execute/Inline should construct a root loop");
        assert_eq!(plan.max_turns, limits.standard_max_turns as usize);
        assert_eq!(
            plan.delegation_max_turns,
            limits.max_model_turns_delegation as usize
        );
        assert!(plan.delegation_max_turns > plan.max_turns);
    }

    #[test]
    fn root_loop_plan_never_widens_an_admitted_resource_slice() {
        // Pins: an experiment child gets one target turn and a small tool allowance;
        // Session configuration and delegation escalation cannot widen either cap.
        let limits = SessionLimitsConfig::default();
        let route = ExecutionRouteDecision::Execute {
            strategy: ExecutionStrategy::Inline,
            rationale: "bounded experiment target".to_string(),
        };
        let plan = root_loop_plan(
            RootLoopPlanRequest {
                route: &route,
                request_max_turns: None,
                resource_budget: ResourceBudget::new(
                    None,
                    Some(ResourceAmounts {
                        cost_micro_usd: 1_000,
                        tokens: 1_000,
                        turns: 1,
                        model_calls: 2,
                        tool_calls: 3,
                    }),
                ),
            },
            &limits,
        )
        .expect("inline route should construct a loop");

        assert_eq!(plan.max_turns, 1);
        assert_eq!(plan.delegation_max_turns, 1);
        assert_eq!(plan.max_tool_calls, 3);
    }

    #[test]
    fn turn_cap_escalation_is_one_way_and_never_lowers() {
        // Pins: a turn that delegates continues past the base cap up to the delegation
        // cap; a turn that never delegates stays at the base cap; the escalation
        // latches on once and never reverses.
        let mut escalation = TurnCapEscalation::new(6, 12);
        assert_eq!(escalation.effective_max_turns(), 6);

        assert!(
            escalation.record_delegation(),
            "the first delegation must transition the cap"
        );
        assert_eq!(escalation.effective_max_turns(), 12);
        assert!(
            !escalation.record_delegation(),
            "a later delegation must not re-transition the already-escalated cap"
        );
        assert_eq!(
            escalation.effective_max_turns(),
            12,
            "the escalated cap never lowers back to the base"
        );

        let undelegated = TurnCapEscalation::new(6, 12);
        assert_eq!(
            undelegated.effective_max_turns(),
            6,
            "a turn with no worker spawn stays bounded by the base cap"
        );
    }

    #[test]
    fn durable_execute_and_needs_input_have_no_model_loop_plan() {
        // Pins: Durable and NeedsInput branch before loop construction and receive no
        // accidental turn or tool budget.
        let limits = SessionLimitsConfig::default();
        for route in [
            ExecutionRouteDecision::Execute {
                strategy: ExecutionStrategy::Durable,
                rationale: "The workflow requires durable execution.".to_string(),
            },
            ExecutionRouteDecision::NeedsInput {
                rationale: "The target is required before work can begin.".to_string(),
                missing_inputs: vec!["target".to_string()],
            },
        ] {
            assert!(
                root_loop_plan(
                    RootLoopPlanRequest {
                        route: &route,
                        request_max_turns: None,
                        resource_budget: ResourceBudget::UNBOUNDED,
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

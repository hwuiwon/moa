//! Deterministic model-loop planning helpers for turn workflows.

use moa_core::config::SessionLimitsConfig;
use moa_core::wire::turn::TurnComplexityClass;

use crate::workflows::turn_responsiveness::{
    ToolBudgetState, TurnResponsivenessInput, classify_turn_request, effective_tool_cap,
    effective_turn_cap,
};

/// Inputs for planning one root session turn loop.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RootLoopPlanRequest<'a> {
    /// User text admitted for this root turn.
    pub(crate) user_text: &'a str,
    /// Number of attachments admitted with this turn.
    pub(crate) attachment_count: usize,
    /// Caller-supplied turn cap.
    pub(crate) request_max_turns: Option<u32>,
    /// Whether persisted recent events point at a target.
    pub(crate) has_recent_target: bool,
    /// Number of available tool schemas before context compilation.
    pub(crate) available_tool_count: usize,
}

/// Inputs for planning one sub-agent turn loop.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SubAgentLoopPlanRequest {
    /// Caller-supplied sub-agent turn cap.
    pub(crate) request_max_turns: Option<u32>,
    /// Default sub-agent workflow cap used when the caller omits one.
    pub(crate) default_max_turns: usize,
}

/// Deterministic model-loop plan shared by workflow shells.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TurnLoopPlan {
    /// Selected responsiveness class.
    pub(crate) complexity_class: TurnComplexityClass,
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
    let complexity_class = classify_turn_request(TurnResponsivenessInput {
        user_text: request.user_text,
        attachment_count: request.attachment_count,
        request_max_turns: request.request_max_turns,
        has_recent_target: request.has_recent_target,
        is_workflow_context: false,
        is_sub_agent_context: false,
        available_tool_count: request.available_tool_count,
    });
    loop_plan(request.request_max_turns, complexity_class, session_limits)
}

/// Builds a deterministic loop plan for a delegated sub-agent turn.
pub(crate) fn sub_agent_loop_plan(
    request: SubAgentLoopPlanRequest,
    session_limits: &SessionLimitsConfig,
) -> TurnLoopPlan {
    let complexity_class = classify_turn_request(TurnResponsivenessInput {
        user_text: "",
        attachment_count: 0,
        request_max_turns: request.request_max_turns,
        has_recent_target: true,
        is_workflow_context: false,
        is_sub_agent_context: true,
        available_tool_count: 0,
    });
    let default_cap = request.default_max_turns.min(u32::MAX as usize) as u32;
    let request_or_default_cap = request.request_max_turns.or(Some(default_cap));
    loop_plan(request_or_default_cap, complexity_class, session_limits)
}

fn loop_plan(
    request_max_turns: Option<u32>,
    complexity_class: TurnComplexityClass,
    session_limits: &SessionLimitsConfig,
) -> TurnLoopPlan {
    let max_turns = effective_turn_cap(request_max_turns, complexity_class, session_limits);
    let max_tool_calls = effective_tool_cap(complexity_class, session_limits);
    TurnLoopPlan {
        complexity_class,
        max_turns,
        max_tool_calls,
        loop_detection_threshold: session_limits.loop_detection_threshold,
    }
}

#[cfg(test)]
mod tests {
    use moa_core::config::SessionLimitsConfig;
    use moa_core::wire::turn::TurnComplexityClass;

    use super::{
        RootLoopPlanRequest, SubAgentLoopPlanRequest, root_loop_plan, sub_agent_loop_plan,
    };

    #[test]
    fn root_vague_turn_uses_clarification_budget() {
        // Pins: root clarification planning stays deterministic outside workflow code.
        let limits = SessionLimitsConfig::default();
        let plan = root_loop_plan(
            RootLoopPlanRequest {
                user_text: "do it",
                attachment_count: 0,
                request_max_turns: None,
                has_recent_target: false,
                available_tool_count: 0,
            },
            &limits,
        );
        assert_eq!(plan.complexity_class, TurnComplexityClass::Clarification);
        assert_eq!(plan.max_turns, limits.simple_max_turns as usize);
        assert_eq!(plan.max_tool_calls, 0);
    }

    #[test]
    fn sub_agent_plan_is_complex_and_defaults_cap() {
        // Pins: sub-agent workflows keep their bounded default loop cap.
        let limits = SessionLimitsConfig::default();
        let plan = sub_agent_loop_plan(
            SubAgentLoopPlanRequest {
                request_max_turns: None,
                default_max_turns: 7,
            },
            &limits,
        );
        assert_eq!(plan.complexity_class, TurnComplexityClass::Complex);
        assert_eq!(plan.max_turns, 7);
        assert_eq!(plan.max_tool_calls, limits.max_tool_calls as usize);
    }
}

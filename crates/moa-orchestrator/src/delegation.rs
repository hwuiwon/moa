//! Helpers for the v2 sub-agent delegation tool surface.

use std::time::Duration;

use moa_core::{
    ListSubAgentsOutput, ListedSubAgent, SpawnSubAgentOutput, SubAgentId, SubAgentState,
    SubAgentStatus, ToolOutput, WaitSubAgentOutput,
};
use serde::Serialize;

/// Maximum wait accepted by the v2 wait tool.
pub(crate) const MAX_WAIT_TIMEOUT_MS: u64 = 30_000;

/// Poll interval used by bounded v2 waits.
pub(crate) const WAIT_POLL_INTERVAL_MS: u64 = 200;

/// Returns whether a sub-agent state is terminal.
#[must_use]
pub(crate) fn is_terminal_sub_agent_state(state: SubAgentState) -> bool {
    matches!(
        state,
        SubAgentState::Completed | SubAgentState::Failed | SubAgentState::Cancelled
    )
}

/// Clamps a model-requested wait timeout to the supported bound.
#[must_use]
pub(crate) fn clamp_wait_timeout_ms(timeout_ms: u64) -> u64 {
    timeout_ms.min(MAX_WAIT_TIMEOUT_MS)
}

/// Converts a status projection into the v2 list entry shape.
#[must_use]
pub(crate) fn listed_sub_agent(sub_agent_id: SubAgentId, status: SubAgentStatus) -> ListedSubAgent {
    ListedSubAgent {
        sub_agent_id,
        state: status.state,
        depth: status.depth,
        tokens_used: status.tokens_used,
        budget_remaining: status.budget_remaining,
    }
}

/// Builds a structured success output for `spawn_sub_agent`.
pub(crate) fn spawn_output(output: SpawnSubAgentOutput) -> ToolOutput {
    json_tool_output(
        format!(
            "Spawned sub-agent {} at {} with status {:?}.",
            output.sub_agent_id, output.path, output.status
        ),
        output,
    )
}

/// Builds a structured success output for `list_sub_agents`.
pub(crate) fn list_output(output: ListSubAgentsOutput) -> ToolOutput {
    let count = output.sub_agents.len();
    json_tool_output(format!("Found {count} child sub-agent(s)."), output)
}

/// Builds a structured success output for `wait_sub_agent`.
pub(crate) fn wait_output(output: WaitSubAgentOutput) -> ToolOutput {
    let summary = if output.timed_out {
        format!(
            "Sub-agent {} is still {:?}; wait timed out.",
            output.sub_agent_id, output.state
        )
    } else {
        format!(
            "Sub-agent {} reached {:?}.",
            output.sub_agent_id, output.state
        )
    };
    json_tool_output(summary, output)
}

/// Builds a structured success output for `message_sub_agent`.
pub(crate) fn message_output(sub_agent_id: &str) -> ToolOutput {
    ToolOutput::text(
        format!("Sent follow-up message to sub-agent {sub_agent_id}."),
        Duration::ZERO,
    )
}

/// Builds a structured success output for `cancel_sub_agent`.
pub(crate) fn cancel_output(sub_agent_id: &str) -> ToolOutput {
    ToolOutput::text(
        format!("Cancellation requested for sub-agent {sub_agent_id}."),
        Duration::ZERO,
    )
}

fn json_tool_output(summary: impl Into<String>, value: impl Serialize) -> ToolOutput {
    let data = serde_json::to_value(value).unwrap_or_else(|error| {
        serde_json::json!({
            "serialization_error": error.to_string()
        })
    });
    ToolOutput::json(summary, data, Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use moa_core::{SubAgentState, SubAgentStatus};

    use super::{
        MAX_WAIT_TIMEOUT_MS, clamp_wait_timeout_ms, is_terminal_sub_agent_state, listed_sub_agent,
    };

    #[test]
    fn wait_timeout_is_clamped_to_supported_bound() {
        // Pins: model-requested waits cannot block a turn longer than the supported max.
        assert_eq!(clamp_wait_timeout_ms(0), 0);
        assert_eq!(clamp_wait_timeout_ms(1_000), 1_000);
        assert_eq!(
            clamp_wait_timeout_ms(MAX_WAIT_TIMEOUT_MS + 1),
            MAX_WAIT_TIMEOUT_MS
        );
    }

    #[test]
    fn terminal_state_detection_matches_sub_agent_lifecycle() {
        // Pins: v2 wait/list behavior agrees on which sub-agent statuses are terminal.
        assert!(!is_terminal_sub_agent_state(SubAgentState::Running));
        assert!(!is_terminal_sub_agent_state(SubAgentState::WaitingApproval));
        assert!(is_terminal_sub_agent_state(SubAgentState::Completed));
        assert!(is_terminal_sub_agent_state(SubAgentState::Failed));
        assert!(is_terminal_sub_agent_state(SubAgentState::Cancelled));
    }

    #[test]
    fn listed_sub_agent_preserves_status_fields() {
        // Pins: list output is a stable projection of child status, not a lossy text summary.
        let listed = listed_sub_agent(
            "child-1".to_string(),
            SubAgentStatus {
                state: SubAgentState::Running,
                depth: 2,
                tokens_used: 11,
                budget_remaining: 22,
                active_children: vec!["grandchild".to_string()],
            },
        );

        assert_eq!(listed.sub_agent_id, "child-1");
        assert_eq!(listed.state, SubAgentState::Running);
        assert_eq!(listed.depth, 2);
        assert_eq!(listed.tokens_used, 11);
        assert_eq!(listed.budget_remaining, 22);
    }
}

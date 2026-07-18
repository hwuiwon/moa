//! Pure execution-run resource and planner defaults.

use serde::{Deserialize, Serialize};

/// Tenant-independent defaults for execution planning and resource envelopes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecutionConfig {
    /// Maximum planner repair calls after the initial candidate.
    pub planner_repair_attempts: u32,
    /// Number of identical failure fingerprints that stops replanning.
    pub repeated_failure_limit: u32,
    /// Default maximum logical tasks in one approved run.
    pub max_tasks: u64,
    /// Default maximum model tokens in one approved run.
    pub max_tokens: u64,
    /// Default maximum governed tool calls in one approved run.
    pub max_tool_calls: u64,
    /// Default maximum retrieved bytes in one approved run.
    pub max_retrieved_bytes: u64,
    /// Default maximum run cost in integer micro-US-dollars.
    pub max_cost_microusd: u64,
    /// Cost threshold above which a run requires owning-user confirmation.
    pub unattended_max_cost_microusd: u64,
    /// Conservative cost estimate for one bounded agent turn.
    pub agent_turn_cost_microusd: u64,
    /// Conservative token estimate for one bounded agent turn.
    pub agent_turn_tokens: u64,
    /// Conservative tool-call estimate for one bounded agent turn.
    pub agent_turn_tool_calls: u64,
    /// Conservative retrieval estimate for one bounded agent turn.
    pub agent_turn_retrieved_bytes: u64,
    /// Conservative cost estimate for one verifier turn.
    pub verifier_turn_cost_microusd: u64,
    /// Conservative token estimate for one verifier turn.
    pub verifier_turn_tokens: u64,
    /// Conservative tool-call estimate for one verifier turn.
    pub verifier_turn_tool_calls: u64,
    /// Conservative retrieval estimate for one verifier turn.
    pub verifier_turn_retrieved_bytes: u64,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            planner_repair_attempts: 1,
            repeated_failure_limit: 3,
            max_tasks: 10_000,
            max_tokens: 10_000_000,
            max_tool_calls: 100_000,
            max_retrieved_bytes: 10_000_000_000,
            max_cost_microusd: 100_000_000,
            unattended_max_cost_microusd: 5_000_000,
            agent_turn_cost_microusd: 100_000,
            agent_turn_tokens: 8_000,
            agent_turn_tool_calls: 8,
            agent_turn_retrieved_bytes: 10_000_000,
            verifier_turn_cost_microusd: 200_000,
            verifier_turn_tokens: 16_000,
            verifier_turn_tool_calls: 4,
            verifier_turn_retrieved_bytes: 1_000_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ExecutionConfig;

    #[test]
    fn execution_config_defaults_match_the_resource_contract() {
        // Pins: run admission and compiler estimates share the documented safety defaults.
        assert_eq!(
            ExecutionConfig::default(),
            ExecutionConfig {
                planner_repair_attempts: 1,
                repeated_failure_limit: 3,
                max_tasks: 10_000,
                max_tokens: 10_000_000,
                max_tool_calls: 100_000,
                max_retrieved_bytes: 10_000_000_000,
                max_cost_microusd: 100_000_000,
                unattended_max_cost_microusd: 5_000_000,
                agent_turn_cost_microusd: 100_000,
                agent_turn_tokens: 8_000,
                agent_turn_tool_calls: 8,
                agent_turn_retrieved_bytes: 10_000_000,
                verifier_turn_cost_microusd: 200_000,
                verifier_turn_tokens: 16_000,
                verifier_turn_tool_calls: 4,
                verifier_turn_retrieved_bytes: 1_000_000,
            }
        );
    }
}

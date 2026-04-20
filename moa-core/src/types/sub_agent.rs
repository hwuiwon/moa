//! Sub-agent message, result, and status types used by Restate orchestration.

use serde::{Deserialize, Serialize};

use super::{ModelId, SessionId, TurnOutcome, UserId, WorkspaceId};

/// Stable sub-agent identifier keyed under the parent session or sub-agent.
pub type SubAgentId = String;

/// One message delivered to a running sub-agent virtual object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubAgentMessage {
    /// Initial task payload used to bootstrap the sub-agent state.
    InitialTask {
        /// Primary task the child should work on.
        task: String,
        /// Tool names the child is allowed to invoke.
        tool_subset: Vec<String>,
        /// Token budget allocated to the child.
        budget_tokens: u64,
        /// Root session that owns the child.
        parent_session: SessionId,
        /// Optional parent sub-agent when dispatch is nested.
        parent_sub_agent: Option<SubAgentId>,
        /// Current depth in the sub-agent tree.
        depth: u32,
        /// Awakeable id the child resolves on terminal completion.
        result_awakeable_id: String,
        /// Workspace scope inherited from the parent.
        workspace_id: WorkspaceId,
        /// User scope inherited from the parent.
        user_id: UserId,
        /// Model inherited from the parent.
        model: ModelId,
    },
    /// Follow-up user-style text delivered from the parent actor.
    FollowUp {
        /// Follow-up text.
        text: String,
    },
    /// Synthetic child-result message reserved for nested fan-out flows.
    ChildResult {
        /// Child that completed.
        sub_agent_id: SubAgentId,
        /// Final child result payload.
        result: SubAgentResult,
    },
}

/// Result resolved back to the parent awakeable when a sub-agent finishes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubAgentResult {
    /// Sub-agent that produced the result.
    pub sub_agent_id: SubAgentId,
    /// Whether the child completed successfully.
    pub success: bool,
    /// Human-readable output returned to the parent.
    pub output: String,
    /// Aggregate tokens consumed by the child.
    pub tokens_used: u64,
    /// Number of tools invoked by the child.
    pub tools_invoked: u32,
    /// Optional terminal error description.
    pub error: Option<String>,
}

/// Read-only sub-agent status returned by the shared status handler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubAgentStatus {
    /// Current lifecycle state.
    pub state: SubAgentState,
    /// Current depth in the child tree.
    pub depth: u32,
    /// Tokens consumed so far.
    pub tokens_used: u64,
    /// Remaining token budget.
    pub budget_remaining: u64,
    /// Active child ids currently owned by the sub-agent.
    pub active_children: Vec<SubAgentId>,
}

/// Lifecycle state tracked for one sub-agent virtual object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubAgentState {
    /// Child is actively running turns.
    Running,
    /// Child is blocked on human approval.
    WaitingApproval,
    /// Child finished successfully.
    Completed,
    /// Child failed terminally.
    Failed,
    /// Child was cancelled.
    Cancelled,
}

/// Persisted child reference used by parents for depth and loop control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubAgentChildRef {
    /// Child object key.
    pub id: SubAgentId,
    /// Stable hash of the active child task and tool subset.
    pub task_hash: String,
}

/// Synthetic dispatch-tool input parsed from provider tool-call JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchSubAgentInput {
    /// Task delegated to the child.
    pub task: String,
    /// Tool names exposed to the child.
    #[serde(default)]
    pub tool_subset: Vec<String>,
    /// Token budget allocated to the child.
    #[serde(default = "default_dispatch_budget_tokens")]
    pub budget_tokens: u64,
}

impl DispatchSubAgentInput {
    /// Converts the dispatch request into the initial child message payload.
    #[allow(clippy::too_many_arguments)]
    pub fn into_initial_message(
        self,
        parent_session: SessionId,
        parent_sub_agent: Option<SubAgentId>,
        depth: u32,
        result_awakeable_id: String,
        workspace_id: WorkspaceId,
        user_id: UserId,
        model: ModelId,
    ) -> SubAgentMessage {
        SubAgentMessage::InitialTask {
            task: self.task,
            tool_subset: self.tool_subset,
            budget_tokens: self.budget_tokens,
            parent_session,
            parent_sub_agent,
            depth,
            result_awakeable_id,
            workspace_id,
            user_id,
            model,
        }
    }
}

/// Stable dispatch-tool schema exposed to provider tool calling.
pub fn dispatch_sub_agent_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "dispatch_sub_agent",
        "description": "Delegate a focused task to a conversational specialist sub-agent and wait for its final result.",
        "input_schema": {
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "Clear delegated task for the child agent."
                },
                "tool_subset": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Subset of tool names the child may use."
                },
                "budget_tokens": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Token budget reserved for the child agent."
                }
            },
            "required": ["task"],
            "additionalProperties": false
        }
    })
}

/// Default token budget reserved for one dispatched child when the model omits it.
pub fn default_dispatch_budget_tokens() -> u64 {
    8_192
}

/// Maps a terminal sub-agent state into the generic turn outcome used by the Session loop.
pub fn turn_outcome_for_sub_agent_state(state: SubAgentState) -> TurnOutcome {
    match state {
        SubAgentState::Running => TurnOutcome::Continue,
        SubAgentState::WaitingApproval => TurnOutcome::WaitingApproval,
        SubAgentState::Completed => TurnOutcome::Idle,
        SubAgentState::Failed | SubAgentState::Cancelled => TurnOutcome::Cancelled,
    }
}

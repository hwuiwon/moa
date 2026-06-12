//! Sub-agent message, result, and status types used by Restate orchestration.

use serde::{Deserialize, Serialize};

use super::{
    CompletionRequest, ModelId, SessionId, SessionMeta, ToolCallId, ToolInvocation, ToolOutput,
    TurnOutcome, UserId, WorkspaceId,
};

/// Stable sub-agent identifier keyed under the parent session or sub-agent.
pub type SubAgentId = String;

/// Stable path-like name for a sub-agent inside one root session tree.
pub type AgentPath = String;

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
    /// Token budget reserved for this child.
    #[serde(default)]
    pub budget_tokens: u64,
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

/// Detached v2 spawn-tool input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnSubAgentInput {
    /// Task delegated to the child.
    pub task: String,
    /// Optional model-visible task name/path segment.
    #[serde(default)]
    pub task_name: Option<String>,
    /// Tool names exposed to the child.
    #[serde(default)]
    pub tool_subset: Vec<String>,
    /// Token budget allocated to the child.
    #[serde(default = "default_dispatch_budget_tokens")]
    pub budget_tokens: u64,
}

impl From<SpawnSubAgentInput> for DispatchSubAgentInput {
    fn from(value: SpawnSubAgentInput) -> Self {
        Self {
            task: value.task,
            tool_subset: value.tool_subset,
            budget_tokens: value.budget_tokens,
        }
    }
}

/// Detached v2 wait-tool input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitSubAgentInput {
    /// Child sub-agent id returned by `spawn_sub_agent`.
    pub sub_agent_id: SubAgentId,
    /// Maximum wait time in milliseconds.
    #[serde(default = "default_wait_timeout_ms")]
    pub timeout_ms: u64,
}

/// Detached v2 message/follow-up input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageSubAgentInput {
    /// Child sub-agent id returned by `spawn_sub_agent`.
    pub sub_agent_id: SubAgentId,
    /// Follow-up text to deliver.
    pub text: String,
}

/// Detached v2 cancellation input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelSubAgentInput {
    /// Child sub-agent id returned by `spawn_sub_agent`.
    pub sub_agent_id: SubAgentId,
    /// Human-readable cancellation reason.
    #[serde(default = "default_cancel_reason")]
    pub reason: String,
}

/// Detached v2 list-tool input.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListSubAgentsInput {}

/// Spawn result returned by the v2 detached tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnSubAgentOutput {
    /// Child sub-agent id.
    pub sub_agent_id: SubAgentId,
    /// Stable model-visible path for the child.
    pub path: AgentPath,
    /// Current child status.
    pub status: SubAgentState,
}

/// One child entry in `list_sub_agents`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListedSubAgent {
    /// Child sub-agent id.
    pub sub_agent_id: SubAgentId,
    /// Current lifecycle state.
    pub state: SubAgentState,
    /// Child tree depth.
    pub depth: u32,
    /// Tokens consumed so far.
    pub tokens_used: u64,
    /// Remaining token budget.
    pub budget_remaining: u64,
}

/// List result returned by `list_sub_agents`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListSubAgentsOutput {
    /// Known child agents owned by the current actor.
    pub sub_agents: Vec<ListedSubAgent>,
}

/// Wait result returned by `wait_sub_agent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitSubAgentOutput {
    /// Child sub-agent id.
    pub sub_agent_id: SubAgentId,
    /// Current lifecycle state.
    pub state: SubAgentState,
    /// Terminal result when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<SubAgentResult>,
    /// Whether the wait timed out before a terminal state.
    pub timed_out: bool,
}

/// Prepared state returned by `SubAgent/prepare_turn` to the turn workflow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SubAgentTurnPreparation {
    /// The child does not need an LLM call for this workflow iteration.
    Outcome {
        /// Immediate turn outcome that the workflow should apply.
        outcome: TurnOutcome,
    },
    /// The child has a compiled completion request ready for the workflow.
    Request {
        /// Completion request to send through `LLMGateway`.
        request: Box<CompletionRequest>,
        /// Active per-turn canary injected into the request when tools were available.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_canary: Option<String>,
        /// Synthetic session metadata used for policies and tool routing.
        session_meta: Box<SessionMeta>,
        /// Root parent session receiving product-visible events.
        parent_session: SessionId,
    },
}

/// Tool-result record applied to a sub-agent's local conversation history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubAgentToolRecord {
    /// Stable tool-call id used in parent session events.
    pub tool_id: ToolCallId,
    /// Provider-emitted invocation payload.
    pub invocation: ToolInvocation,
    /// Tool output visible to the child model on the next turn.
    pub output: ToolOutput,
}

/// Request to reserve a child sub-agent under another sub-agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReserveSubAgentInput {
    /// Dispatch-shaped child request.
    pub request: DispatchSubAgentInput,
    /// Optional model-visible child task name for path generation.
    #[serde(default)]
    pub task_name: Option<String>,
    /// Awakeable id to resolve when the child finishes, or empty for detached children.
    #[serde(default)]
    pub result_awakeable_id: String,
}

/// Child reservation returned after a parent sub-agent admits a child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReservedSubAgent {
    /// Durable child registry entry held by the parent sub-agent.
    pub child_ref: SubAgentChildRef,
    /// Initial message the workflow should send to the child object.
    pub initial_message: SubAgentMessage,
    /// Stable path returned to model-visible detached spawn calls.
    pub path: AgentPath,
    /// Original delegated task recorded in parent events.
    pub task: String,
    /// Token budget reserved for the child.
    pub budget_tokens: u64,
}

/// Request to remove a completed child from a parent sub-agent registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompleteSubAgentChildInput {
    /// Child sub-agent id to remove.
    pub sub_agent_id: SubAgentId,
    /// Tokens consumed by the child so unused budget can be refunded.
    pub tokens_used: u64,
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

/// Stable `spawn_sub_agent` tool schema.
pub fn spawn_sub_agent_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "spawn_sub_agent",
        "description": "Start a focused child sub-agent for bounded independent work and return immediately with its id.",
        "input_schema": {
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "Clear delegated task for the child agent."
                },
                "task_name": {
                    "type": "string",
                    "description": "Short optional model-visible name for the child task."
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

/// Stable `wait_sub_agent` tool schema.
pub fn wait_sub_agent_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "wait_sub_agent",
        "description": "Wait briefly for a previously spawned sub-agent to finish and return its current status or terminal result.",
        "input_schema": {
            "type": "object",
            "properties": {
                "sub_agent_id": {
                    "type": "string",
                    "description": "Sub-agent id returned by spawn_sub_agent."
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 30000,
                    "description": "Maximum wait time in milliseconds."
                }
            },
            "required": ["sub_agent_id"],
            "additionalProperties": false
        }
    })
}

/// Stable `message_sub_agent` tool schema.
pub fn message_sub_agent_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "message_sub_agent",
        "description": "Send a follow-up instruction to a running or resident sub-agent.",
        "input_schema": {
            "type": "object",
            "properties": {
                "sub_agent_id": {
                    "type": "string",
                    "description": "Sub-agent id returned by spawn_sub_agent."
                },
                "text": {
                    "type": "string",
                    "description": "Follow-up instruction for the child agent."
                }
            },
            "required": ["sub_agent_id", "text"],
            "additionalProperties": false
        }
    })
}

/// Stable `list_sub_agents` tool schema.
pub fn list_sub_agents_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "list_sub_agents",
        "description": "List child sub-agents owned by the current agent and their current statuses.",
        "input_schema": {
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }
    })
}

/// Stable `cancel_sub_agent` tool schema.
pub fn cancel_sub_agent_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "cancel_sub_agent",
        "description": "Cancel a previously spawned child sub-agent.",
        "input_schema": {
            "type": "object",
            "properties": {
                "sub_agent_id": {
                    "type": "string",
                    "description": "Sub-agent id returned by spawn_sub_agent."
                },
                "reason": {
                    "type": "string",
                    "description": "Short cancellation reason."
                }
            },
            "required": ["sub_agent_id"],
            "additionalProperties": false
        }
    })
}

/// Returns all v2 delegation tool schemas.
pub fn delegation_tool_schemas() -> Vec<serde_json::Value> {
    vec![
        spawn_sub_agent_tool_schema(),
        wait_sub_agent_tool_schema(),
        message_sub_agent_tool_schema(),
        list_sub_agents_tool_schema(),
        cancel_sub_agent_tool_schema(),
    ]
}

/// Returns one v2 delegation tool schema by name.
pub fn delegation_tool_schema(name: &str) -> Option<serde_json::Value> {
    match name {
        "spawn_sub_agent" => Some(spawn_sub_agent_tool_schema()),
        "wait_sub_agent" => Some(wait_sub_agent_tool_schema()),
        "message_sub_agent" => Some(message_sub_agent_tool_schema()),
        "list_sub_agents" => Some(list_sub_agents_tool_schema()),
        "cancel_sub_agent" => Some(cancel_sub_agent_tool_schema()),
        _ => None,
    }
}

/// Returns whether `name` is one of MOA's built-in delegation tools.
pub fn is_delegation_tool_name(name: &str) -> bool {
    matches!(
        name,
        "dispatch_sub_agent"
            | "spawn_sub_agent"
            | "wait_sub_agent"
            | "message_sub_agent"
            | "list_sub_agents"
            | "cancel_sub_agent"
    )
}

/// Default token budget reserved for one dispatched child when the model omits it.
pub fn default_dispatch_budget_tokens() -> u64 {
    8_192
}

/// Default bounded wait for the v2 wait tool.
pub fn default_wait_timeout_ms() -> u64 {
    5_000
}

fn default_cancel_reason() -> String {
    "cancelled by parent".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_delegation_schema_names_are_stable() {
        // Pins: the model-facing v2 delegation tool names remain stable.
        let names = delegation_tool_schemas()
            .into_iter()
            .map(|schema| {
                schema
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .expect("delegation schema should have a string name")
                    .to_string()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec![
                "spawn_sub_agent",
                "wait_sub_agent",
                "message_sub_agent",
                "list_sub_agents",
                "cancel_sub_agent",
            ]
        );
    }

    #[test]
    fn spawn_input_converts_to_legacy_dispatch_input() {
        // Pins: the compatibility path preserves task, tools, and budget when v2 spawn wraps v1 dispatch state.
        let spawn = SpawnSubAgentInput {
            task: "inspect docs".to_string(),
            task_name: Some("docs".to_string()),
            tool_subset: vec!["file_read".to_string()],
            budget_tokens: 512,
        };

        let dispatch = DispatchSubAgentInput::from(spawn);

        assert_eq!(
            dispatch,
            DispatchSubAgentInput {
                task: "inspect docs".to_string(),
                tool_subset: vec!["file_read".to_string()],
                budget_tokens: 512,
            }
        );
    }
}

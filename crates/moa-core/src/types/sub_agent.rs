//! Sub-agent message, result, and status types used by Restate orchestration.

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::error::{MoaError, Result};

use super::{
    CompletionRequest, CompletionResponse, ModelId, SessionId, SessionMeta, ToolCallId,
    ToolInvocation, ToolOutput, TurnOutcome, UserId, WorkspaceId,
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
        /// Optional maximum autonomous turns for the child.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_turns: Option<u32>,
        /// Root session that owns the child.
        parent_session: SessionId,
        /// Optional parent sub-agent when dispatch is nested.
        parent_sub_agent: Option<SubAgentId>,
        /// Current depth in the sub-agent tree.
        depth: u32,
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
    /// Child key exists but has not received its initial task payload.
    Uninitialized,
    /// Child is actively running turns.
    Running,
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
    /// Terminal result cached on the parent until a wait or dispatch consumes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<SubAgentTerminalResult>,
}

/// Terminal child state and result delivered from a sub-agent to its parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubAgentTerminalResult {
    /// Final lifecycle state observed for the child.
    pub state: SubAgentState,
    /// Final child output payload.
    pub result: SubAgentResult,
}

/// Child sub-agent request stored during parent reservation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubAgentChildRequest {
    /// Task delegated to the child.
    pub task: String,
    /// Tool names exposed to the child.
    #[serde(default)]
    pub tool_subset: Vec<String>,
    /// Token budget allocated to the child.
    #[serde(default = "default_sub_agent_budget_tokens")]
    pub budget_tokens: u64,
    /// Optional maximum autonomous turns for the child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
}

/// Spawn-tool input.
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
    #[serde(default = "default_sub_agent_budget_tokens")]
    pub budget_tokens: u64,
    /// Optional maximum autonomous turns for the child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
}

/// Wait-tool input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitSubAgentInput {
    /// Child sub-agent id returned by `spawn_sub_agent`.
    pub sub_agent_id: SubAgentId,
    /// Maximum wait time in milliseconds.
    #[serde(default = "default_wait_timeout_ms")]
    pub timeout_ms: u64,
}

/// Message/follow-up input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageSubAgentInput {
    /// Child sub-agent id returned by `spawn_sub_agent`.
    pub sub_agent_id: SubAgentId,
    /// Follow-up text to deliver.
    pub text: String,
}

/// Cancellation input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelSubAgentInput {
    /// Child sub-agent id returned by `spawn_sub_agent`.
    pub sub_agent_id: SubAgentId,
    /// Human-readable cancellation reason.
    #[serde(default = "default_cancel_reason")]
    pub reason: String,
}

/// List-tool input.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListSubAgentsInput {}

/// Spawn result returned by `spawn_sub_agent`.
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

/// Input for registering an awakeable that should resolve when a child terminates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachSubAgentResultWaiterInput {
    /// Awakeable id owned by the waiting workflow.
    pub awakeable_id: String,
}

/// Output returned when registering a terminal result waiter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachSubAgentResultWaiterOutput {
    /// Already available terminal result, if the child had finished before registration.
    pub terminal: Option<SubAgentTerminalResult>,
}

/// Input for removing a terminal result waiter after timeout or cancellation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveSubAgentResultWaiterInput {
    /// Awakeable id that should no longer be resolved by the child.
    pub awakeable_id: String,
}

/// Input for caching a child's terminal result on its parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkSubAgentChildTerminalInput {
    /// Child sub-agent id.
    pub sub_agent_id: SubAgentId,
    /// Terminal state and result to cache.
    pub terminal: SubAgentTerminalResult,
}

/// Input for consuming a cached child result from a parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsumeSubAgentChildResultInput {
    /// Child sub-agent id.
    pub sub_agent_id: SubAgentId,
}

/// Output returned when consuming a cached child result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsumeSubAgentChildResultOutput {
    /// Terminal result, if one was cached and consumed.
    pub terminal: Option<SubAgentTerminalResult>,
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

/// Turn-scoped LLM response record applied to a sub-agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubAgentTurnResponseRecord {
    /// Workflow turn id that produced the response.
    pub turn_id: String,
    /// LLM response to append to child-local history.
    pub response: CompletionResponse,
}

/// Tool-result record applied to a sub-agent's local conversation history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubAgentToolRecord {
    /// Workflow turn id that produced the tool result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// Stable tool-call id used in parent session events.
    pub tool_id: ToolCallId,
    /// Provider-emitted invocation payload.
    pub invocation: ToolInvocation,
    /// Tool output visible to the child model on the next turn.
    pub output: ToolOutput,
}

/// Turn-scoped core outcome applied to a sub-agent lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubAgentTurnOutcomeRecord {
    /// Workflow turn id that produced the outcome.
    pub turn_id: String,
    /// Core turn outcome to apply.
    pub outcome: TurnOutcome,
}

/// Request to reserve a child sub-agent under another sub-agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReserveSubAgentInput {
    /// Child request to reserve.
    pub request: SubAgentChildRequest,
    /// Optional model-visible child task name for path generation.
    #[serde(default)]
    pub task_name: Option<String>,
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

/// Stable kind for one built-in sub-agent delegation tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationToolKind {
    /// Child spawn tool.
    Spawn,
    /// Child wait tool.
    Wait,
    /// Follow-up message tool.
    Message,
    /// Child-listing tool.
    List,
    /// Child cancellation tool.
    Cancel,
}

impl DelegationToolKind {
    /// All built-in delegation tool kinds in stable prompt order.
    pub const ALL: [Self; 5] = [
        Self::Spawn,
        Self::Wait,
        Self::Message,
        Self::List,
        Self::Cancel,
    ];

    /// Returns the stable provider-facing tool name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Spawn => "spawn_sub_agent",
            Self::Wait => "wait_sub_agent",
            Self::Message => "message_sub_agent",
            Self::List => "list_sub_agents",
            Self::Cancel => "cancel_sub_agent",
        }
    }

    /// Returns the kind for a provider-facing delegation tool name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// Returns the provider-facing JSON schema for this tool.
    #[must_use]
    pub fn schema(self) -> serde_json::Value {
        match self {
            Self::Spawn => spawn_sub_agent_tool_schema(),
            Self::Wait => wait_sub_agent_tool_schema(),
            Self::Message => message_sub_agent_tool_schema(),
            Self::List => list_sub_agents_tool_schema(),
            Self::Cancel => cancel_sub_agent_tool_schema(),
        }
    }
}

/// Parsed payload for one built-in sub-agent delegation tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegationTool {
    /// Child spawn payload.
    Spawn(SpawnSubAgentInput),
    /// Child wait payload.
    Wait(WaitSubAgentInput),
    /// Follow-up message payload.
    Message(MessageSubAgentInput),
    /// Child-listing payload.
    List(ListSubAgentsInput),
    /// Child cancellation payload.
    Cancel(CancelSubAgentInput),
}

impl DelegationTool {
    /// Parses a provider invocation into a typed delegation tool when recognized.
    pub fn from_invocation(invocation: &ToolInvocation) -> Result<Option<Self>> {
        let Some(kind) = DelegationToolKind::from_name(&invocation.name) else {
            return Ok(None);
        };

        Ok(Some(match kind {
            DelegationToolKind::Spawn => Self::Spawn(parse_delegation_tool_input(invocation)?),
            DelegationToolKind::Wait => Self::Wait(parse_delegation_tool_input(invocation)?),
            DelegationToolKind::Message => Self::Message(parse_delegation_tool_input(invocation)?),
            DelegationToolKind::List => Self::List(parse_delegation_tool_input(invocation)?),
            DelegationToolKind::Cancel => Self::Cancel(parse_delegation_tool_input(invocation)?),
        }))
    }

    /// Returns the parsed tool kind.
    #[must_use]
    pub fn kind(&self) -> DelegationToolKind {
        match self {
            Self::Spawn(_) => DelegationToolKind::Spawn,
            Self::Wait(_) => DelegationToolKind::Wait,
            Self::Message(_) => DelegationToolKind::Message,
            Self::List(_) => DelegationToolKind::List,
            Self::Cancel(_) => DelegationToolKind::Cancel,
        }
    }

    /// Returns the stable provider-facing tool name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.kind().name()
    }
}

impl SubAgentChildRequest {
    /// Converts the child request into the initial child message payload.
    #[allow(clippy::too_many_arguments)]
    pub fn into_initial_message(
        self,
        parent_session: SessionId,
        parent_sub_agent: Option<SubAgentId>,
        depth: u32,
        workspace_id: WorkspaceId,
        user_id: UserId,
        model: ModelId,
    ) -> SubAgentMessage {
        SubAgentMessage::InitialTask {
            task: self.task,
            tool_subset: self.tool_subset,
            budget_tokens: self.budget_tokens,
            max_turns: self.max_turns,
            parent_session,
            parent_sub_agent,
            depth,
            workspace_id,
            user_id,
            model,
        }
    }
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
                },
                "max_turns": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum autonomous turns the child may run for this task."
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

/// Returns all delegation tool schemas.
pub fn delegation_tool_schemas() -> Vec<serde_json::Value> {
    DelegationToolKind::ALL
        .into_iter()
        .map(DelegationToolKind::schema)
        .collect()
}

/// Returns one delegation tool schema by name.
pub fn delegation_tool_schema(name: &str) -> Option<serde_json::Value> {
    DelegationToolKind::from_name(name).map(DelegationToolKind::schema)
}

/// Returns whether `name` is one of MOA's built-in delegation tools.
pub fn is_delegation_tool_name(name: &str) -> bool {
    DelegationToolKind::from_name(name).is_some()
}

/// Parses a delegation tool invocation input while preserving the tool name in errors.
pub fn parse_delegation_tool_input<T>(invocation: &ToolInvocation) -> Result<T>
where
    T: DeserializeOwned,
{
    serde_json::from_value(invocation.input.clone()).map_err(|error| {
        MoaError::SerializationError(format!(
            "failed to deserialize {} input: {error}",
            invocation.name
        ))
    })
}

/// Default token budget reserved for one child when the model omits it.
pub fn default_sub_agent_budget_tokens() -> u64 {
    8_192
}

/// Default bounded wait for the wait tool.
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
    fn delegation_schema_names_are_stable() {
        // Pins: the model-facing delegation tool names remain stable.
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
    fn stable_delegation_names_map_to_expected_kind() {
        // Pins: each stable tool name remains classified under the intended delegation kind.
        let expected = [
            ("spawn_sub_agent", DelegationToolKind::Spawn),
            ("wait_sub_agent", DelegationToolKind::Wait),
            ("message_sub_agent", DelegationToolKind::Message),
            ("list_sub_agents", DelegationToolKind::List),
            ("cancel_sub_agent", DelegationToolKind::Cancel),
        ];

        for (name, expected_kind) in expected {
            assert!(
                is_delegation_tool_name(name),
                "{name} should be recognized as a delegation tool"
            );
            let observed_kind = DelegationToolKind::from_name(name)
                .unwrap_or_else(|| panic!("{name} should map to a delegation kind"));
            assert_eq!(observed_kind, expected_kind, "{name} kind changed");
        }

        assert!(!is_delegation_tool_name("unknown_sub_agent"));
        assert!(delegation_tool_schema("unknown_sub_agent").is_none());
    }

    #[test]
    fn known_delegation_tool_parse_error_names_tool() {
        // Pins: delegation input parsing errors identify the offending tool call.
        let invocation = ToolInvocation {
            id: Some("toolu_1".to_string()),
            name: "spawn_sub_agent".to_string(),
            input: serde_json::json!("not an object"),
        };

        let error = parse_delegation_tool_input::<SpawnSubAgentInput>(&invocation)
            .expect_err("invalid spawn_sub_agent input should fail");

        let message = error.to_string();
        assert!(
            message.contains("spawn_sub_agent"),
            "error should name the tool, got: {message}"
        );
        assert!(
            message.contains("failed to deserialize"),
            "error should describe a parse failure, got: {message}"
        );
    }

    #[test]
    fn typed_delegation_parser_covers_every_builtin_tool() {
        // Pins: every built-in delegation tool has exactly one typed payload branch.
        let cases = [
            (
                "spawn_sub_agent",
                serde_json::json!({
                    "task": "research",
                    "task_name": "research-task",
                    "tool_subset": ["web_fetch"],
                    "budget_tokens": 123
                }),
                DelegationToolKind::Spawn,
            ),
            (
                "wait_sub_agent",
                serde_json::json!({
                    "sub_agent_id": "child-1",
                    "timeout_ms": 50
                }),
                DelegationToolKind::Wait,
            ),
            (
                "message_sub_agent",
                serde_json::json!({
                    "sub_agent_id": "child-1",
                    "text": "continue"
                }),
                DelegationToolKind::Message,
            ),
            (
                "list_sub_agents",
                serde_json::json!({}),
                DelegationToolKind::List,
            ),
            (
                "cancel_sub_agent",
                serde_json::json!({
                    "sub_agent_id": "child-1",
                    "reason": "no longer needed"
                }),
                DelegationToolKind::Cancel,
            ),
        ];

        for (name, input, expected_kind) in cases {
            let invocation = ToolInvocation {
                id: Some(format!("{name}-id")),
                name: name.to_string(),
                input,
            };
            let parsed = DelegationTool::from_invocation(&invocation)
                .expect("known delegation tool should parse")
                .unwrap_or_else(|| panic!("{name} should be recognized"));

            assert_eq!(parsed.kind(), expected_kind, "{name} parsed to wrong kind");
            assert_eq!(parsed.name(), name);
        }
    }

    #[test]
    fn typed_delegation_parser_ignores_unknown_tools() {
        // Pins: non-delegation tools stay on the regular tool-executor path.
        let invocation = ToolInvocation {
            id: Some("regular-tool".to_string()),
            name: "bash".to_string(),
            input: serde_json::json!({"cmd": "true"}),
        };

        assert_eq!(
            DelegationTool::from_invocation(&invocation).expect("unknown tool should not fail"),
            None
        );
    }
}

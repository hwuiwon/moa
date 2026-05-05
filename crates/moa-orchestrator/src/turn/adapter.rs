//! Agent-specific hooks for the shared durable turn runner.

use moa_core::{
    ActiveSegment, CompletionRequest, CompletionResponse, DispatchSubAgentInput, SessionId,
    SessionMeta, SubAgentId, ToolCallId, ToolInvocation, ToolOutput, TurnOutcome,
};
use restate_sdk::prelude::*;

use crate::sub_agent_dispatch::DispatchedSubAgent;

/// Per-agent behavior required by the shared turn runner.
#[allow(async_fn_in_trait)]
pub(crate) trait AgentAdapter: Send + Sync {
    /// Returns the durable children-list key for this agent.
    fn children_state_key(&self) -> &'static str;

    /// Returns the durable budget key when the agent tracks remaining budget.
    fn budget_state_key(&self) -> Option<&'static str> {
        None
    }

    /// Returns the current sub-agent identifier for event tagging, when present.
    fn sub_agent_id(&self, ctx: &ObjectContext<'_>) -> Option<SubAgentId>;

    /// Returns whether the agent was cancelled before or during the turn.
    async fn is_cancelled(&self, ctx: &ObjectContext<'_>) -> Result<bool, HandlerError>;

    /// Returns whether the agent is currently blocked on an approval decision.
    async fn has_pending_approval(&self, ctx: &ObjectContext<'_>) -> Result<bool, HandlerError>;

    /// Enforces any per-agent depth or budget limits before the turn starts.
    async fn enforce_limits(&self, _ctx: &ObjectContext<'_>) -> Result<(), HandlerError> {
        Ok(())
    }

    /// Builds the next completion request, or `None` when the agent is idle.
    async fn build_request(
        &self,
        ctx: &ObjectContext<'_>,
    ) -> Result<Option<CompletionRequest>, HandlerError>;

    /// Returns session metadata for policy lookups and event attribution.
    async fn session_meta(&self, ctx: &ObjectContext<'_>) -> Result<SessionMeta, HandlerError>;

    /// Returns the prompt text associated with the turn, when available.
    async fn turn_prompt(&self, _ctx: &ObjectContext<'_>) -> Result<Option<String>, HandlerError> {
        Ok(None)
    }

    /// Returns the owning root session for event persistence.
    async fn owning_session_id(&self, ctx: &ObjectContext<'_>) -> Result<SessionId, HandlerError>;

    /// Applies a turn outcome to the agent's durable lifecycle state.
    async fn apply_outcome(
        &self,
        ctx: &ObjectContext<'_>,
        outcome: TurnOutcome,
    ) -> Result<(), HandlerError>;

    /// Emits a structured error event when max turns is exceeded.
    async fn emit_turn_budget_exceeded(
        &self,
        ctx: &ObjectContext<'_>,
        max_turns: usize,
    ) -> Result<(), HandlerError>;

    /// Records the raw LLM response in local state before tool execution begins.
    async fn record_response(
        &self,
        ctx: &ObjectContext<'_>,
        response: &CompletionResponse,
    ) -> Result<(), HandlerError>;

    /// Records one executed tool result in agent-local state.
    async fn record_tool_result(
        &self,
        ctx: &ObjectContext<'_>,
        tool_id: ToolCallId,
        invocation: &ToolInvocation,
        output: &ToolOutput,
    ) -> Result<(), HandlerError>;

    /// Returns the current active task segment, if any.
    async fn current_segment(
        &self,
        _ctx: &ObjectContext<'_>,
    ) -> Result<Option<ActiveSegment>, HandlerError> {
        Ok(None)
    }

    /// Records that a tool was used in the current task segment.
    async fn record_segment_tool_use(
        &self,
        _ctx: &ObjectContext<'_>,
        _tool_name: &str,
    ) -> Result<(), HandlerError> {
        Ok(())
    }

    /// Records that a skill was activated in the current task segment.
    async fn record_segment_skill_activation(
        &self,
        _ctx: &ObjectContext<'_>,
        _skill_name: &str,
    ) -> Result<(), HandlerError> {
        Ok(())
    }

    /// Records a denied tool result in agent-local state.
    async fn record_denied_tool(
        &self,
        ctx: &ObjectContext<'_>,
        tool_id: ToolCallId,
        invocation: &ToolInvocation,
        output: &ToolOutput,
    ) -> Result<(), HandlerError>;

    /// Drains pending messages into the request/history inputs before a turn.
    async fn drain_pending_before_request(
        &self,
        ctx: &ObjectContext<'_>,
    ) -> Result<(), HandlerError>;

    /// Dispatches a child sub-agent and waits durably for its result.
    async fn dispatch_child(
        &self,
        ctx: &mut ObjectContext<'_>,
        input: DispatchSubAgentInput,
    ) -> Result<DispatchedSubAgent, HandlerError>;

    /// Marks the agent as blocked on the given approval awakeable.
    async fn set_pending_approval(
        &self,
        ctx: &ObjectContext<'_>,
        awakeable_id: String,
    ) -> Result<(), HandlerError>;

    /// Clears the current pending approval marker after the gate resumes.
    async fn clear_pending_approval(&self, ctx: &ObjectContext<'_>) -> Result<(), HandlerError>;
}

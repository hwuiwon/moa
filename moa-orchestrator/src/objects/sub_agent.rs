//! Restate virtual object that owns one durable conversational sub-agent.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use chrono::Utc;
use moa_core::{
    ApprovalDecision, CompletionContent, CompletionRequest, ContextMessage, Event, MoaError,
    ModelCapabilities, ModelId, PolicyAction, SessionId, SessionMeta, SessionStatus, StopReason,
    SubAgentChildRef, SubAgentId, SubAgentMessage, SubAgentResult, SubAgentState, SubAgentStatus,
    ToolCallContent, ToolCallId, ToolCallRequest, ToolInvocation, ToolOutput, TurnOutcome, UserId,
    UserMessage, WorkspaceId, dispatch_sub_agent_tool_schema, record_approval_wait,
};
use restate_sdk::prelude::*;
use serde_json::json;
use uuid::Uuid;

use crate::observability::annotate_restate_handler_span;
use crate::runtime::{PROVIDERS, TOOL_SCHEMAS};
use crate::services::llm_gateway::LLMGatewayClient;
use crate::services::session_store::{AppendEventRequest, SessionStoreClient};
use crate::services::tool_executor::ToolExecutorClient;
use crate::services::workspace_store::{
    PrepareToolApprovalRequest, StoreApprovalRuleRequest, WorkspaceStoreClient,
};
use crate::sub_agent_dispatch::{
    MAX_SUB_AGENT_DEPTH, dispatch_sub_agent, sub_agent_result_tool_output,
};

const K_STATUS: &str = "status";
const K_PENDING: &str = "pending";
const K_PENDING_APPROVAL: &str = "pending_approval";
const K_CHILDREN: &str = "children";
const K_LAST_TURN_SUMMARY: &str = "last_turn_summary";
const K_PARENT_SESSION: &str = "parent_session";
const K_PARENT_SUB_AGENT: &str = "parent_sub_agent";
const K_DEPTH: &str = "depth";
const K_BUDGET_REMAINING: &str = "budget_remaining";
const K_TOKENS_USED: &str = "tokens_used";
const K_RESULT_AWAKEABLE_ID: &str = "result_awakeable_id";
const K_TASK: &str = "task";
const K_TOOL_SUBSET: &str = "tool_subset";
const K_WORKSPACE_ID: &str = "workspace_id";
const K_USER_ID: &str = "user_id";
const K_MODEL: &str = "model";
const K_HISTORY: &str = "history";
const K_TOOLS_INVOKED: &str = "tools_invoked";
const K_CANCEL_REASON: &str = "cancel_reason";
const APPROVAL_TIMEOUT_SECS_ENV: &str = "MOA_APPROVAL_TIMEOUT_SECS";
const DEFAULT_APPROVAL_TIMEOUT_SECS: u64 = 30 * 60;
const MAX_TURNS_PER_POST: usize = 50;

/// Serializable projection of the SubAgent VO's durable state keys.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SubAgentVoState {
    /// Current lifecycle state.
    pub status: Option<SubAgentState>,
    /// Root session that owns this child.
    pub parent_session: Option<SessionId>,
    /// Optional parent child when this is a nested sub-agent.
    pub parent_sub_agent: Option<SubAgentId>,
    /// Current depth in the child tree.
    pub depth: u32,
    /// Remaining token budget for future turns.
    pub budget_remaining: u64,
    /// Aggregate tokens consumed so far.
    pub tokens_used: u64,
    /// Awakeable identifier resolved on terminal completion.
    pub result_awakeable_id: Option<String>,
    /// Original delegated task.
    pub task: Option<String>,
    /// Tool names the child may invoke.
    pub tool_subset: Vec<String>,
    /// Workspace scope inherited from the parent.
    pub workspace_id: Option<WorkspaceId>,
    /// User scope inherited from the parent.
    pub user_id: Option<UserId>,
    /// Model inherited from the parent.
    pub model: Option<ModelId>,
    /// Buffered parent messages waiting for the next turn.
    pub pending: Vec<UserMessage>,
    /// Buffered conversation history carried across turns.
    pub history: Vec<ContextMessage>,
    /// Pending approval awakeable when blocked.
    pub pending_approval: Option<String>,
    /// Child sub-agents currently owned by this sub-agent.
    pub children: Vec<SubAgentChildRef>,
    /// Summary of the last assistant response.
    pub last_turn_summary: Option<String>,
    /// Number of tools invoked so far.
    pub tools_invoked: u32,
    /// Cooperative cancellation reason, when requested.
    pub cancel_reason: Option<String>,
}

impl SubAgentVoState {
    /// Bootstraps state from the initial parent-dispatch payload.
    pub fn initialize(&mut self, msg: &SubAgentMessage) -> moa_core::Result<()> {
        let SubAgentMessage::InitialTask {
            task,
            tool_subset,
            budget_tokens,
            parent_session,
            parent_sub_agent,
            depth,
            result_awakeable_id,
            workspace_id,
            user_id,
            model,
        } = msg
        else {
            return Err(MoaError::ValidationError(
                "sub-agent initialization requires an InitialTask message".to_string(),
            ));
        };

        self.status = Some(SubAgentState::Running);
        self.parent_session = Some(*parent_session);
        self.parent_sub_agent = parent_sub_agent.clone();
        self.depth = *depth;
        self.budget_remaining = *budget_tokens;
        self.tokens_used = 0;
        self.result_awakeable_id = Some(result_awakeable_id.clone());
        self.task = Some(task.clone());
        self.tool_subset = tool_subset.clone();
        self.workspace_id = Some(workspace_id.clone());
        self.user_id = Some(user_id.clone());
        self.model = Some(model.clone());
        self.pending = vec![UserMessage {
            text: task.clone(),
            attachments: Vec::new(),
        }];
        self.history.clear();
        self.pending_approval = None;
        self.children.clear();
        self.last_turn_summary = None;
        self.tools_invoked = 0;
        self.cancel_reason = None;
        Ok(())
    }

    /// Returns the current lifecycle state, defaulting to `Completed` when empty.
    #[must_use]
    pub fn current_status(&self) -> SubAgentState {
        self.status.unwrap_or(SubAgentState::Completed)
    }

    /// Ensures the child was initialized before handling follow-up messages or turns.
    pub fn ensure_initialized(&self) -> moa_core::Result<()> {
        if self.parent_session.is_some()
            && self.task.is_some()
            && self.workspace_id.is_some()
            && self.user_id.is_some()
            && self.model.is_some()
        {
            return Ok(());
        }

        Err(MoaError::ValidationError(
            "sub-agent state is not initialized".to_string(),
        ))
    }

    /// Queues a follow-up message and transitions the child into `Running`.
    pub fn enqueue_follow_up(&mut self, text: String) -> moa_core::Result<()> {
        self.ensure_initialized()?;
        self.pending.push(UserMessage {
            text,
            attachments: Vec::new(),
        });
        self.status = Some(SubAgentState::Running);
        Ok(())
    }

    /// Applies the latest turn outcome to the lifecycle state.
    pub fn apply_turn_outcome(&mut self, outcome: TurnOutcome) -> SubAgentState {
        let state = match outcome {
            TurnOutcome::Continue => SubAgentState::Running,
            TurnOutcome::Idle => SubAgentState::Completed,
            TurnOutcome::WaitingApproval => SubAgentState::WaitingApproval,
            TurnOutcome::Cancelled => SubAgentState::Cancelled,
        };
        self.status = Some(state);
        state
    }

    /// Records new token usage and deducts it from the remaining budget.
    pub fn record_token_usage(&mut self, used: u64) {
        self.tokens_used = self.tokens_used.saturating_add(used);
        self.budget_remaining = self.budget_remaining.saturating_sub(used);
    }

    /// Returns whether the child has exhausted its local token budget.
    #[must_use]
    pub fn budget_exhausted(&self) -> bool {
        self.budget_remaining == 0
    }

    /// Builds the public status projection returned by the shared status handler.
    #[must_use]
    pub fn status_view(&self) -> SubAgentStatus {
        SubAgentStatus {
            state: self.current_status(),
            depth: self.depth,
            tokens_used: self.tokens_used,
            budget_remaining: self.budget_remaining,
            active_children: self.children.iter().map(|child| child.id.clone()).collect(),
        }
    }

    /// Builds the final payload resolved back to the parent awakeable.
    #[must_use]
    pub fn build_result(&self, sub_agent_id: SubAgentId) -> SubAgentResult {
        let success = matches!(self.current_status(), SubAgentState::Completed);
        let output = self
            .last_turn_summary
            .clone()
            .or_else(|| latest_assistant_text(&self.history))
            .unwrap_or_else(|| self.task.clone().unwrap_or_default());
        let error = match self.current_status() {
            SubAgentState::Completed => None,
            SubAgentState::Cancelled => Some(
                self.cancel_reason
                    .clone()
                    .unwrap_or_else(|| "sub-agent cancelled".to_string()),
            ),
            SubAgentState::Failed => Some("sub-agent failed".to_string()),
            SubAgentState::Running | SubAgentState::WaitingApproval => {
                Some("sub-agent finished before reaching a terminal state".to_string())
            }
        };

        SubAgentResult {
            sub_agent_id,
            success,
            output,
            tokens_used: self.tokens_used,
            tools_invoked: self.tools_invoked,
            error,
        }
    }
}

/// Restate virtual object surface for one conversational sub-agent.
#[restate_sdk::object]
pub trait SubAgent {
    /// Parent dispatches a message (initial task or follow-up).
    async fn post_message(msg: Json<SubAgentMessage>) -> Result<(), HandlerError>;

    /// Returns read-only child status without entering the single-writer queue.
    #[shared]
    async fn status() -> Result<Json<SubAgentStatus>, HandlerError>;

    /// Requests cooperative cancellation for the child.
    async fn cancel(reason: String) -> Result<(), HandlerError>;

    /// Resolves the currently pending approval decision for the child.
    #[shared]
    async fn approve(decision: Json<ApprovalDecision>) -> Result<(), HandlerError>;

    /// Runs one conversational turn for the child.
    async fn run_turn() -> Result<Json<TurnOutcome>, HandlerError>;

    /// Clears all persisted state for this child key.
    async fn destroy() -> Result<(), HandlerError>;
}

/// Concrete `SubAgent` virtual object implementation.
pub struct SubAgentImpl;

impl SubAgent for SubAgentImpl {
    #[tracing::instrument(skip(self, ctx, msg))]
    async fn post_message(
        &self,
        mut ctx: ObjectContext<'_>,
        msg: Json<SubAgentMessage>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "post_message");
        let message = msg.into_inner();
        let mut state = load_object_state(&ctx).await?;
        match &message {
            SubAgentMessage::InitialTask { .. } => {
                state.initialize(&message).map_err(to_handler_error)?;
            }
            SubAgentMessage::FollowUp { text } => {
                state
                    .enqueue_follow_up(text.clone())
                    .map_err(to_handler_error)?;
            }
            SubAgentMessage::ChildResult {
                sub_agent_id,
                result,
            } => {
                state
                    .enqueue_follow_up(format!(
                        "Child sub-agent {sub_agent_id} completed.\n{}",
                        result.output
                    ))
                    .map_err(to_handler_error)?;
            }
        }
        persist_state(&ctx, &state);

        let mut turns_this_post = 0usize;
        loop {
            turns_this_post += 1;
            if turns_this_post > MAX_TURNS_PER_POST {
                let mut current = load_object_state(&ctx).await?;
                current.status = Some(SubAgentState::Failed);
                current.last_turn_summary =
                    Some(format!("turn budget exceeded ({MAX_TURNS_PER_POST})"));
                persist_state(&ctx, &current);
                break;
            }

            let outcome = run_turn_once(&mut ctx).await?;
            let mut current = load_object_state(&ctx).await?;
            current.apply_turn_outcome(outcome);
            persist_state(&ctx, &current);

            match outcome {
                TurnOutcome::Continue => continue,
                TurnOutcome::Idle | TurnOutcome::WaitingApproval | TurnOutcome::Cancelled => {
                    break;
                }
            }
        }

        maybe_resolve_parent_awakeable(&ctx).await
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn status(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<SubAgentStatus>, HandlerError> {
        annotate_restate_handler_span("SubAgent", "status");
        Ok(Json::from(load_shared_state(&ctx).await?.status_view()))
    }

    #[tracing::instrument(skip(self, ctx, reason))]
    async fn cancel(&self, ctx: ObjectContext<'_>, reason: String) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "cancel");
        let mut state = load_object_state(&ctx).await?;
        state.cancel_reason = Some(reason.clone());
        state.status = Some(SubAgentState::Cancelled);
        let children = state.children.clone();
        persist_state(&ctx, &state);

        for child in children {
            ctx.object_client::<SubAgentClient>(child.id)
                .cancel(reason.clone())
                .send();
        }
        tracing::info!(key = %ctx.key(), %reason, "sub-agent cancel requested");
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, decision))]
    async fn approve(
        &self,
        ctx: SharedObjectContext<'_>,
        decision: Json<ApprovalDecision>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "approve");
        let awakeable_id = ctx
            .get::<Json<String>>(K_PENDING_APPROVAL)
            .await?
            .map(Json::into_inner)
            .ok_or_else(|| TerminalError::new("no pending approval for this sub-agent"))?;
        let serialized_decision = serialize_awakeable_decision(&decision.into_inner())?;
        ctx.resolve_awakeable(&awakeable_id, serialized_decision);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn run_turn(
        &self,
        mut ctx: ObjectContext<'_>,
    ) -> Result<Json<TurnOutcome>, HandlerError> {
        annotate_restate_handler_span("SubAgent", "run_turn");
        Ok(Json::from(run_turn_once(&mut ctx).await?))
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn destroy(&self, ctx: ObjectContext<'_>) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "destroy");
        ctx.clear_all();
        tracing::info!(key = %ctx.key(), "sub-agent VO state cleared");
        Ok(())
    }
}

async fn load_object_state(ctx: &ObjectContext<'_>) -> Result<SubAgentVoState, HandlerError> {
    Ok(SubAgentVoState {
        status: ctx
            .get::<Json<SubAgentState>>(K_STATUS)
            .await?
            .map(Json::into_inner),
        parent_session: ctx
            .get::<Json<SessionId>>(K_PARENT_SESSION)
            .await?
            .map(Json::into_inner),
        parent_sub_agent: ctx
            .get::<Json<SubAgentId>>(K_PARENT_SUB_AGENT)
            .await?
            .map(Json::into_inner),
        depth: ctx
            .get::<Json<u32>>(K_DEPTH)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default(),
        budget_remaining: ctx
            .get::<Json<u64>>(K_BUDGET_REMAINING)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default(),
        tokens_used: ctx
            .get::<Json<u64>>(K_TOKENS_USED)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default(),
        result_awakeable_id: ctx
            .get::<Json<String>>(K_RESULT_AWAKEABLE_ID)
            .await?
            .map(Json::into_inner),
        task: ctx.get::<Json<String>>(K_TASK).await?.map(Json::into_inner),
        tool_subset: ctx
            .get::<Json<Vec<String>>>(K_TOOL_SUBSET)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default(),
        workspace_id: ctx
            .get::<Json<WorkspaceId>>(K_WORKSPACE_ID)
            .await?
            .map(Json::into_inner),
        user_id: ctx
            .get::<Json<UserId>>(K_USER_ID)
            .await?
            .map(Json::into_inner),
        model: ctx
            .get::<Json<ModelId>>(K_MODEL)
            .await?
            .map(Json::into_inner),
        pending: ctx
            .get::<Json<Vec<UserMessage>>>(K_PENDING)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default(),
        history: ctx
            .get::<Json<Vec<ContextMessage>>>(K_HISTORY)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default(),
        pending_approval: ctx
            .get::<Json<String>>(K_PENDING_APPROVAL)
            .await?
            .map(Json::into_inner),
        children: ctx
            .get::<Json<Vec<SubAgentChildRef>>>(K_CHILDREN)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default(),
        last_turn_summary: ctx
            .get::<Json<String>>(K_LAST_TURN_SUMMARY)
            .await?
            .map(Json::into_inner),
        tools_invoked: ctx
            .get::<Json<u32>>(K_TOOLS_INVOKED)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default(),
        cancel_reason: ctx
            .get::<Json<String>>(K_CANCEL_REASON)
            .await?
            .map(Json::into_inner),
    })
}

async fn load_shared_state(ctx: &SharedObjectContext<'_>) -> Result<SubAgentVoState, HandlerError> {
    Ok(SubAgentVoState {
        status: ctx
            .get::<Json<SubAgentState>>(K_STATUS)
            .await?
            .map(Json::into_inner),
        parent_session: ctx
            .get::<Json<SessionId>>(K_PARENT_SESSION)
            .await?
            .map(Json::into_inner),
        parent_sub_agent: ctx
            .get::<Json<SubAgentId>>(K_PARENT_SUB_AGENT)
            .await?
            .map(Json::into_inner),
        depth: ctx
            .get::<Json<u32>>(K_DEPTH)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default(),
        budget_remaining: ctx
            .get::<Json<u64>>(K_BUDGET_REMAINING)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default(),
        tokens_used: ctx
            .get::<Json<u64>>(K_TOKENS_USED)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default(),
        result_awakeable_id: ctx
            .get::<Json<String>>(K_RESULT_AWAKEABLE_ID)
            .await?
            .map(Json::into_inner),
        task: ctx.get::<Json<String>>(K_TASK).await?.map(Json::into_inner),
        tool_subset: ctx
            .get::<Json<Vec<String>>>(K_TOOL_SUBSET)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default(),
        workspace_id: ctx
            .get::<Json<WorkspaceId>>(K_WORKSPACE_ID)
            .await?
            .map(Json::into_inner),
        user_id: ctx
            .get::<Json<UserId>>(K_USER_ID)
            .await?
            .map(Json::into_inner),
        model: ctx
            .get::<Json<ModelId>>(K_MODEL)
            .await?
            .map(Json::into_inner),
        pending: ctx
            .get::<Json<Vec<UserMessage>>>(K_PENDING)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default(),
        history: ctx
            .get::<Json<Vec<ContextMessage>>>(K_HISTORY)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default(),
        pending_approval: ctx
            .get::<Json<String>>(K_PENDING_APPROVAL)
            .await?
            .map(Json::into_inner),
        children: ctx
            .get::<Json<Vec<SubAgentChildRef>>>(K_CHILDREN)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default(),
        last_turn_summary: ctx
            .get::<Json<String>>(K_LAST_TURN_SUMMARY)
            .await?
            .map(Json::into_inner),
        tools_invoked: ctx
            .get::<Json<u32>>(K_TOOLS_INVOKED)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default(),
        cancel_reason: ctx
            .get::<Json<String>>(K_CANCEL_REASON)
            .await?
            .map(Json::into_inner),
    })
}

fn persist_state(ctx: &ObjectContext<'_>, state: &SubAgentVoState) {
    set_or_clear(ctx, K_STATUS, state.status);
    set_or_clear_ref(ctx, K_PARENT_SESSION, state.parent_session.as_ref());
    set_or_clear_ref(ctx, K_PARENT_SUB_AGENT, state.parent_sub_agent.as_ref());
    set_or_clear_scalar(ctx, K_DEPTH, state.depth, state.depth == 0);
    set_or_clear_scalar(
        ctx,
        K_BUDGET_REMAINING,
        state.budget_remaining,
        state.budget_remaining == 0,
    );
    set_or_clear_scalar(
        ctx,
        K_TOKENS_USED,
        state.tokens_used,
        state.tokens_used == 0,
    );
    set_or_clear_ref(
        ctx,
        K_RESULT_AWAKEABLE_ID,
        state.result_awakeable_id.as_ref(),
    );
    set_or_clear_ref(ctx, K_TASK, state.task.as_ref());
    set_or_clear_vec(ctx, K_TOOL_SUBSET, &state.tool_subset);
    set_or_clear_ref(ctx, K_WORKSPACE_ID, state.workspace_id.as_ref());
    set_or_clear_ref(ctx, K_USER_ID, state.user_id.as_ref());
    set_or_clear_ref(ctx, K_MODEL, state.model.as_ref());
    set_or_clear_vec(ctx, K_PENDING, &state.pending);
    set_or_clear_vec(ctx, K_HISTORY, &state.history);
    set_or_clear_ref(ctx, K_PENDING_APPROVAL, state.pending_approval.as_ref());
    set_or_clear_vec(ctx, K_CHILDREN, &state.children);
    set_or_clear_ref(ctx, K_LAST_TURN_SUMMARY, state.last_turn_summary.as_ref());
    set_or_clear_scalar(
        ctx,
        K_TOOLS_INVOKED,
        state.tools_invoked,
        state.tools_invoked == 0,
    );
    set_or_clear_ref(ctx, K_CANCEL_REASON, state.cancel_reason.as_ref());
}

fn set_or_clear<T>(ctx: &ObjectContext<'_>, key: &str, value: Option<T>)
where
    T: serde::Serialize + 'static,
{
    match value {
        Some(value) => ctx.set(key, Json::from(value)),
        None => ctx.clear(key),
    }
}

fn set_or_clear_ref<T>(ctx: &ObjectContext<'_>, key: &str, value: Option<&T>)
where
    T: Clone + serde::Serialize + 'static,
{
    match value {
        Some(value) => ctx.set(key, Json::from(value.clone())),
        None => ctx.clear(key),
    }
}

fn set_or_clear_vec<T>(ctx: &ObjectContext<'_>, key: &str, value: &[T])
where
    T: Clone + serde::Serialize + 'static,
{
    if value.is_empty() {
        ctx.clear(key);
    } else {
        ctx.set(key, Json::from(value.to_vec()));
    }
}

fn set_or_clear_scalar<T>(ctx: &ObjectContext<'_>, key: &str, value: T, should_clear: bool)
where
    T: serde::Serialize + 'static,
{
    if should_clear {
        ctx.clear(key);
    } else {
        ctx.set(key, Json::from(value));
    }
}

async fn run_turn_once(ctx: &mut ObjectContext<'_>) -> Result<TurnOutcome, HandlerError> {
    let mut state = load_object_state(ctx).await?;
    state.ensure_initialized().map_err(to_handler_error)?;

    if state.cancel_reason.is_some() {
        return Ok(TurnOutcome::Cancelled);
    }
    if state.pending_approval.is_some() {
        return Ok(TurnOutcome::WaitingApproval);
    }
    if state.depth >= MAX_SUB_AGENT_DEPTH {
        return Err(TerminalError::new(format!(
            "sub-agent depth exceeds maximum ({MAX_SUB_AGENT_DEPTH})"
        ))
        .into());
    }
    if state.pending.is_empty() || state.budget_exhausted() {
        return Ok(TurnOutcome::Idle);
    }

    let pending = std::mem::take(&mut state.pending);
    for message in &pending {
        state
            .history
            .push(ContextMessage::user(render_user_message(message)));
    }
    persist_state(ctx, &state);

    let mut request = build_completion_request(&state)?;
    request.messages.extend(state.history.clone());
    let response = ctx
        .service_client::<LLMGatewayClient>()
        .complete(Json::from(request))
        .call()
        .await?
        .into_inner();

    let token_usage = response.token_usage();
    state.record_token_usage((token_usage.total_input_tokens() + token_usage.output_tokens) as u64);
    state.last_turn_summary = summarize_response_text(&response);
    apply_response_to_history(&mut state.history, &response);
    persist_state(ctx, &state);

    for (index, tool_call) in response_tool_calls(&response).iter().enumerate() {
        if load_object_state(ctx).await?.cancel_reason.is_some() {
            return Ok(TurnOutcome::Cancelled);
        }

        let current = load_object_state(ctx).await?;
        let parent_session = current.parent_session.ok_or_else(|| {
            TerminalError::new("sub-agent parent session missing while dispatching tool")
        })?;
        let workspace_id = current
            .workspace_id
            .clone()
            .ok_or_else(|| TerminalError::new("sub-agent workspace_id missing"))?;
        let user_id = current
            .user_id
            .clone()
            .ok_or_else(|| TerminalError::new("sub-agent user_id missing"))?;
        let model = current
            .model
            .clone()
            .ok_or_else(|| TerminalError::new("sub-agent model missing"))?;
        let tool_id =
            stable_tool_call_id(parent_session, current.depth as usize + index, tool_call);
        let invocation = tool_call.invocation.clone();

        if invocation.name == "dispatch_sub_agent" {
            append_parent_event(
                ctx,
                parent_session,
                Event::ToolCall {
                    tool_id,
                    provider_tool_use_id: invocation.id.clone(),
                    provider_thought_signature: tool_call
                        .provider_metadata
                        .as_ref()
                        .and_then(|metadata| metadata.thought_signature())
                        .map(str::to_string),
                    tool_name: invocation.name.clone(),
                    input: invocation.input.clone(),
                    hand_id: None,
                },
            )
            .await?;

            let dispatch_input: moa_core::DispatchSubAgentInput =
                serde_json::from_value(invocation.input.clone()).map_err(|error| {
                    TerminalError::new(format!(
                        "failed to deserialize dispatch_sub_agent input: {error}"
                    ))
                })?;
            let dispatched = dispatch_sub_agent(
                ctx,
                K_CHILDREN,
                Some(K_BUDGET_REMAINING),
                parent_session,
                Some(ctx.key().to_string()),
                current.depth,
                dispatch_input,
                workspace_id,
                user_id,
                model,
            )
            .await?;
            let output = sub_agent_result_tool_output(&dispatched.result);
            append_parent_event(
                ctx,
                parent_session,
                Event::ToolResult {
                    tool_id,
                    provider_tool_use_id: invocation.id.clone(),
                    output: output.clone(),
                    original_output_tokens: output.original_output_tokens,
                    success: !output.is_error,
                    duration_ms: 0,
                },
            )
            .await?;

            let mut current = load_object_state(ctx).await?;
            current.history.push(ContextMessage::assistant_tool_call(
                ToolInvocation {
                    id: invocation.id.clone(),
                    name: invocation.name.clone(),
                    input: invocation.input.clone(),
                },
                format!(
                    "Dispatching sub-agent for {}",
                    dispatched.result.sub_agent_id
                ),
            ));
            current.history.push(ContextMessage::tool_result(
                invocation
                    .id
                    .clone()
                    .unwrap_or_else(|| tool_id.0.to_string()),
                output.to_text(),
                Some(output.content.clone()),
            ));
            current.tools_invoked = current.tools_invoked.saturating_add(1);
            persist_state(ctx, &current);
            continue;
        }

        let session_meta = synthetic_session_meta(&current)?;
        let approval = ctx
            .service_client::<WorkspaceStoreClient>()
            .prepare_tool_approval(Json(PrepareToolApprovalRequest {
                session: session_meta.clone(),
                invocation: invocation.clone(),
                request_id: tool_id.0,
            }))
            .call()
            .await?
            .into_inner();

        if matches!(approval.action, PolicyAction::Deny) {
            append_parent_event(
                ctx,
                parent_session,
                Event::ToolError {
                    tool_id,
                    provider_tool_use_id: invocation.id.clone(),
                    tool_name: invocation.name.clone(),
                    error: format!("tool {} denied by policy", invocation.name),
                    retryable: false,
                },
            )
            .await?;
            continue;
        }

        if matches!(approval.action, PolicyAction::RequireApproval) {
            let prompt = approval.prompt.ok_or_else(|| {
                TerminalError::new(format!(
                    "workspace store did not return an approval prompt for tool {}",
                    invocation.name
                ))
            })?;
            let (awakeable_id, awakeable) = ctx.awakeable::<String>();
            let mut waiting_state = load_object_state(ctx).await?;
            waiting_state.pending_approval = Some(awakeable_id.clone());
            waiting_state.status = Some(SubAgentState::WaitingApproval);
            persist_state(ctx, &waiting_state);

            let mut prompt = prompt;
            prompt.request.sub_agent_id = Some(ctx.key().to_string());
            append_parent_event(
                ctx,
                parent_session,
                Event::ApprovalRequested {
                    request_id: prompt.request.request_id,
                    awakeable_id: Some(awakeable_id.clone()),
                    sub_agent_id: Some(ctx.key().to_string()),
                    tool_name: prompt.request.tool_name.clone(),
                    input_summary: prompt.request.input_summary.clone(),
                    risk_level: prompt.request.risk_level.clone(),
                    prompt,
                },
            )
            .await?;

            let timeout = approval_wait_timeout();
            let timeout_reason = format!(
                "Auto-denied: no decision within {} minutes",
                timeout.as_secs() / 60
            );
            let approval_started = std::time::Instant::now();
            let decision = restate_sdk::select! {
                decision = awakeable => {
                    parse_awakeable_decision(&decision?)?
                },
                _ = ctx.sleep(timeout) => {
                    ApprovalDecision::Deny {
                        reason: Some(timeout_reason.clone()),
                    }
                }
            };
            record_approval_wait(
                approval_started.elapsed(),
                match &decision {
                    ApprovalDecision::AllowOnce => "allow_once",
                    ApprovalDecision::AlwaysAllow { .. } => "always_allow",
                    ApprovalDecision::Deny {
                        reason: Some(reason),
                    } if reason == &timeout_reason => "timeout",
                    ApprovalDecision::Deny { .. } => "deny",
                },
            );

            let mut resumed_state = load_object_state(ctx).await?;
            resumed_state.pending_approval = None;
            resumed_state.status = Some(SubAgentState::Running);
            persist_state(ctx, &resumed_state);

            let decided_by = match &decision {
                ApprovalDecision::Deny {
                    reason: Some(reason),
                } if reason == &timeout_reason => "system:auto-timeout".to_string(),
                _ => session_meta.user_id.to_string(),
            };
            append_parent_event(
                ctx,
                parent_session,
                Event::ApprovalDecided {
                    request_id: tool_id.0,
                    sub_agent_id: Some(ctx.key().to_string()),
                    decision: decision.clone(),
                    decided_by,
                    decided_at: Utc::now(),
                },
            )
            .await?;

            match decision {
                ApprovalDecision::AllowOnce => {}
                ApprovalDecision::AlwaysAllow { pattern } => {
                    ctx.service_client::<WorkspaceStoreClient>()
                        .store_approval_rule(Json(StoreApprovalRuleRequest {
                            session: session_meta.clone(),
                            tool_name: invocation.name.clone(),
                            pattern,
                            action: PolicyAction::Allow,
                            created_by: session_meta.user_id.clone(),
                        }))
                        .call()
                        .await?;
                }
                ApprovalDecision::Deny { reason } => {
                    let output = ToolOutput::error(
                        format!(
                            "Tool execution denied: {}",
                            reason.unwrap_or_else(|| "Denied by the user".to_string())
                        ),
                        Duration::ZERO,
                    );
                    append_parent_event(
                        ctx,
                        parent_session,
                        Event::ToolResult {
                            tool_id,
                            provider_tool_use_id: invocation.id.clone(),
                            output: output.clone(),
                            original_output_tokens: output.original_output_tokens,
                            success: false,
                            duration_ms: 0,
                        },
                    )
                    .await?;

                    let mut current = load_object_state(ctx).await?;
                    current.history.push(ContextMessage::assistant_tool_call(
                        ToolInvocation {
                            id: invocation.id.clone(),
                            name: invocation.name.clone(),
                            input: invocation.input.clone(),
                        },
                        format!("Approval required for {}", invocation.name),
                    ));
                    current.history.push(ContextMessage::tool_result(
                        invocation
                            .id
                            .clone()
                            .unwrap_or_else(|| tool_id.0.to_string()),
                        output.to_text(),
                        Some(output.content.clone()),
                    ));
                    persist_state(ctx, &current);
                    continue;
                }
            }
        }

        let output = ctx
            .service_client::<ToolExecutorClient>()
            .execute(Json::from(ToolCallRequest {
                tool_call_id: tool_id,
                provider_tool_use_id: invocation.id.clone(),
                tool_name: invocation.name.clone(),
                input: invocation.input.clone(),
                session_id: Some(parent_session),
                workspace_id: session_meta.workspace_id.clone(),
                user_id: session_meta.user_id.clone(),
                idempotency_key: invocation.id.clone(),
            }))
            .call()
            .await?
            .into_inner();

        let mut current = load_object_state(ctx).await?;
        current.history.push(ContextMessage::assistant_tool_call(
            ToolInvocation {
                id: invocation.id.clone(),
                name: invocation.name.clone(),
                input: invocation.input.clone(),
            },
            format!("Calling tool {}", invocation.name),
        ));
        current.history.push(ContextMessage::tool_result(
            invocation
                .id
                .clone()
                .unwrap_or_else(|| tool_id.0.to_string()),
            output.to_text(),
            Some(output.content.clone()),
        ));
        current.tools_invoked = current.tools_invoked.saturating_add(1);
        persist_state(ctx, &current);
    }

    Ok(turn_outcome_for_response(&response))
}

async fn maybe_resolve_parent_awakeable(ctx: &ObjectContext<'_>) -> Result<(), HandlerError> {
    let mut state = load_object_state(ctx).await?;
    if !matches!(
        state.current_status(),
        SubAgentState::Completed | SubAgentState::Failed | SubAgentState::Cancelled
    ) {
        return Ok(());
    }

    let Some(awakeable_id) = state.result_awakeable_id.clone() else {
        return Ok(());
    };

    let payload =
        serde_json::to_string(&state.build_result(ctx.key().to_string())).map_err(|error| {
            TerminalError::new(format!("failed to serialize sub-agent result: {error}"))
        })?;
    ctx.resolve_awakeable(&awakeable_id, payload);
    state.result_awakeable_id = None;
    persist_state(ctx, &state);
    Ok(())
}

fn build_completion_request(state: &SubAgentVoState) -> Result<CompletionRequest, HandlerError> {
    let model = state
        .model
        .clone()
        .ok_or_else(|| TerminalError::new("sub-agent model missing"))?;
    let capabilities = configured_model_capabilities(&model)?;
    let mut request = CompletionRequest {
        model: Some(model),
        messages: vec![ContextMessage::system(sub_agent_system_prompt(state))],
        tools: filtered_tool_schemas(&state.tool_subset)?,
        max_output_tokens: Some(capabilities.max_output),
        temperature: None,
        cache_breakpoints: Vec::new(),
        cache_controls: Vec::new(),
        metadata: HashMap::new(),
    };
    request
        .metadata
        .insert("_moa.sub_agent_id".to_string(), json!(state.task_hash()));
    Ok(request)
}

fn filtered_tool_schemas(tool_subset: &[String]) -> Result<Vec<serde_json::Value>, HandlerError> {
    let configured = TOOL_SCHEMAS
        .get()
        .cloned()
        .ok_or_else(|| TerminalError::new("orchestrator tool schemas not initialized"))?;
    let allowed = tool_subset
        .iter()
        .cloned()
        .collect::<std::collections::HashSet<_>>();
    let mut tools = configured
        .iter()
        .filter(|schema| {
            schema
                .get("name")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|name| allowed.contains(name))
        })
        .cloned()
        .collect::<Vec<_>>();
    if allowed.contains("dispatch_sub_agent") {
        tools.push(dispatch_sub_agent_tool_schema());
    }
    Ok(tools)
}

fn configured_model_capabilities(model: &ModelId) -> Result<ModelCapabilities, HandlerError> {
    PROVIDERS
        .get()
        .ok_or_else(|| TerminalError::new("orchestrator provider registry not initialized"))?
        .capabilities_for_model(Some(model.as_str()))
        .map_err(to_handler_error)
}

fn synthetic_session_meta(state: &SubAgentVoState) -> Result<SessionMeta, HandlerError> {
    Ok(SessionMeta {
        id: state
            .parent_session
            .ok_or_else(|| TerminalError::new("sub-agent parent session missing"))?,
        workspace_id: state
            .workspace_id
            .clone()
            .ok_or_else(|| TerminalError::new("sub-agent workspace_id missing"))?,
        user_id: state
            .user_id
            .clone()
            .ok_or_else(|| TerminalError::new("sub-agent user_id missing"))?,
        model: state
            .model
            .clone()
            .ok_or_else(|| TerminalError::new("sub-agent model missing"))?,
        status: SessionStatus::Running,
        updated_at: Utc::now(),
        ..SessionMeta::default()
    })
}

async fn append_parent_event(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    event: Event,
) -> Result<(), HandlerError> {
    ctx.service_client::<SessionStoreClient>()
        .append_event(Json(AppendEventRequest { session_id, event }))
        .call()
        .await?;
    Ok(())
}

fn apply_response_to_history(
    history: &mut Vec<ContextMessage>,
    response: &moa_core::CompletionResponse,
) {
    let mut appended_text = false;
    for block in &response.content {
        match block {
            CompletionContent::Text(text) if !text.trim().is_empty() => {
                history.push(ContextMessage::assistant_with_thought_signature(
                    text.clone(),
                    response.thought_signature.clone(),
                ));
                appended_text = true;
            }
            CompletionContent::ToolCall(tool_call) => {
                history.push(ContextMessage::assistant_tool_call_with_thought_signature(
                    tool_call.invocation.clone(),
                    if response.text.trim().is_empty() {
                        format!("Calling tool {}", tool_call.invocation.name)
                    } else {
                        response.text.clone()
                    },
                    response.thought_signature.clone(),
                ));
            }
            CompletionContent::ProviderToolResult { tool_name, summary } => {
                history.push(ContextMessage::assistant(format!("{tool_name}: {summary}")));
                appended_text = true;
            }
            CompletionContent::Text(_) => {}
        }
    }

    if !appended_text
        && !response.text.trim().is_empty()
        && response_tool_calls(response).is_empty()
    {
        history.push(ContextMessage::assistant_with_thought_signature(
            response.text.clone(),
            response.thought_signature.clone(),
        ));
    }
}

fn response_tool_calls(response: &moa_core::CompletionResponse) -> Vec<&ToolCallContent> {
    response
        .content
        .iter()
        .filter_map(|block| match block {
            CompletionContent::ToolCall(tool_call) => Some(tool_call),
            CompletionContent::Text(_) | CompletionContent::ProviderToolResult { .. } => None,
        })
        .collect()
}

fn turn_outcome_for_response(response: &moa_core::CompletionResponse) -> TurnOutcome {
    if !response_tool_calls(response).is_empty() || response.stop_reason == StopReason::ToolUse {
        return TurnOutcome::Continue;
    }

    if response.stop_reason == StopReason::Cancelled {
        return TurnOutcome::Cancelled;
    }

    TurnOutcome::Idle
}

fn summarize_response_text(response: &moa_core::CompletionResponse) -> Option<String> {
    let trimmed = response.text.trim();
    if trimmed.is_empty() {
        return None;
    }

    Some(trimmed.chars().take(240).collect())
}

fn render_user_message(message: &UserMessage) -> String {
    if message.attachments.is_empty() {
        return message.text.clone();
    }

    let attachments = message
        .attachments
        .iter()
        .map(|attachment| attachment.name.clone())
        .collect::<Vec<_>>()
        .join(", ");
    format!("{}\n\nAttachments: {attachments}", message.text)
}

fn sub_agent_system_prompt(state: &SubAgentVoState) -> String {
    let task = state
        .task
        .as_deref()
        .unwrap_or("Complete the delegated task.");
    let tools = if state.tool_subset.is_empty() {
        "No tools are available.".to_string()
    } else {
        format!("Allowed tools: {}", state.tool_subset.join(", "))
    };
    format!(
        "You are a specialist sub-agent working for a parent MOA session.\n\
         Complete the delegated task precisely and return a concise final result to the parent.\n\
         Task: {task}\n\
         {tools}"
    )
}

fn latest_assistant_text(history: &[ContextMessage]) -> Option<String> {
    history
        .iter()
        .rev()
        .find(|message| {
            matches!(message.role, moa_core::MessageRole::Assistant)
                && !message.content.trim().is_empty()
        })
        .map(|message| message.content.clone())
}

fn stable_tool_call_id(
    session_id: SessionId,
    index: usize,
    tool_call: &ToolCallContent,
) -> ToolCallId {
    if let Some(raw_id) = tool_call.invocation.id.as_deref()
        && let Ok(uuid) = Uuid::parse_str(raw_id)
    {
        return ToolCallId(uuid);
    }

    let mut hasher = DefaultHasher::new();
    session_id.hash(&mut hasher);
    index.hash(&mut hasher);
    tool_call.invocation.name.hash(&mut hasher);
    tool_call.invocation.input.to_string().hash(&mut hasher);
    ToolCallId(Uuid::from_u128(hasher.finish() as u128))
}

fn approval_wait_timeout() -> Duration {
    approval_wait_timeout_from_env(
        std::env::var(APPROVAL_TIMEOUT_SECS_ENV).ok().as_deref(),
        DEFAULT_APPROVAL_TIMEOUT_SECS,
    )
}

fn approval_wait_timeout_from_env(raw: Option<&str>, default_secs: u64) -> Duration {
    raw.and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(default_secs))
}

fn serialize_awakeable_decision(decision: &ApprovalDecision) -> Result<String, TerminalError> {
    serde_json::to_string(decision).map_err(|error| {
        TerminalError::new(format!(
            "failed to serialize approval decision for awakeable: {error}"
        ))
    })
}

fn parse_awakeable_decision(raw: &str) -> Result<ApprovalDecision, TerminalError> {
    serde_json::from_str(raw).map_err(|error| {
        TerminalError::new(format!(
            "failed to deserialize approval decision from awakeable: {error}"
        ))
    })
}

fn to_handler_error(error: MoaError) -> HandlerError {
    if error.is_fatal() {
        return TerminalError::new(error.to_string()).into();
    }

    HandlerError::from(error)
}

impl SubAgentVoState {
    fn task_hash(&self) -> String {
        let mut hasher = DefaultHasher::new();
        self.task.hash(&mut hasher);
        let mut tools = self.tool_subset.clone();
        tools.sort();
        tools.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }
}

#[cfg(test)]
mod tests {
    use moa_core::{ModelId, SessionId, SubAgentMessage, UserId, WorkspaceId};

    use super::{SubAgentState, SubAgentVoState};

    fn initial_task() -> SubAgentMessage {
        SubAgentMessage::InitialTask {
            task: "summarize repo status".to_string(),
            tool_subset: vec!["web_fetch".to_string()],
            budget_tokens: 512,
            parent_session: SessionId::new(),
            parent_sub_agent: None,
            depth: 1,
            result_awakeable_id: "awake-1".to_string(),
            workspace_id: WorkspaceId::new("workspace-1"),
            user_id: UserId::new("user-1"),
            model: ModelId::new("test-model"),
        }
    }

    #[test]
    fn initial_task_seeds_state() {
        let mut state = SubAgentVoState::default();
        state
            .initialize(&initial_task())
            .expect("initial task should seed state");

        assert_eq!(state.current_status(), SubAgentState::Running);
        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.tool_subset, vec!["web_fetch".to_string()]);
        assert_eq!(state.budget_remaining, 512);
    }

    #[test]
    fn follow_up_queues_message() {
        let mut state = SubAgentVoState::default();
        state
            .initialize(&initial_task())
            .expect("initial task should seed state");
        state
            .enqueue_follow_up("continue".to_string())
            .expect("follow-up should queue");

        assert_eq!(state.pending.len(), 2);
        assert_eq!(state.pending[1].text, "continue");
    }

    #[test]
    fn token_usage_reduces_budget() {
        let mut state = SubAgentVoState::default();
        state
            .initialize(&initial_task())
            .expect("initial task should seed state");
        state.record_token_usage(200);

        assert_eq!(state.tokens_used, 200);
        assert_eq!(state.budget_remaining, 312);
        assert!(!state.budget_exhausted());
    }

    #[test]
    fn build_result_uses_terminal_state() {
        let mut state = SubAgentVoState::default();
        state
            .initialize(&initial_task())
            .expect("initial task should seed state");
        state.status = Some(SubAgentState::Completed);
        state.last_turn_summary = Some("finished".to_string());
        let result = state.build_result("parent-1-child-1".to_string());

        assert!(result.success);
        assert_eq!(result.output, "finished");
    }
}

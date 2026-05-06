//! Restate virtual object that owns one durable conversational sub-agent.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use chrono::Utc;
use moa_core::{
    ActiveSegment, ApprovalDecision, CompletionRequest, CompletionResponse, ContextMessage,
    DispatchSubAgentInput, Event, MoaError, ModelCapabilities, ModelId, SessionId, SessionMeta,
    SessionStatus, SubAgentChildRef, SubAgentId, SubAgentMessage, SubAgentResult, SubAgentState,
    SubAgentStatus, ToolCallId, ToolInvocation, ToolOutput, TurnOutcome, UserId, UserMessage,
    WorkspaceId, dispatch_sub_agent_tool_schema,
};
use restate_sdk::prelude::*;
use serde_json::json;

use crate::OrchestratorCtx;
use crate::observability::annotate_restate_handler_span;
use crate::services::session_store::{
    AppendEventRequest, RecordSegmentToolUseRequest, RecordSegmentTurnUsageRequest,
    RestateSessionStoreClient,
};
use crate::sub_agent_dispatch::{DispatchedSubAgent, MAX_SUB_AGENT_DEPTH, dispatch_sub_agent};
use crate::turn::approval::serialize_awakeable_decision;
use crate::turn::util::{
    apply_response_to_history, dispatch_history_text, summarize_response_text,
};
use crate::turn::{AgentAdapter, TurnRunner};
use crate::vo::{VoReader, VoState, set_or_clear_opt, set_or_clear_scalar, set_or_clear_vec};

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
    fn current_status(&self) -> SubAgentState {
        self.status.unwrap_or(SubAgentState::Completed)
    }

    /// Ensures the child was initialized before handling follow-up messages or turns.
    fn ensure_initialized(&self) -> moa_core::Result<()> {
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
    fn enqueue_follow_up(&mut self, text: String) -> moa_core::Result<()> {
        self.ensure_initialized()?;
        self.pending.push(UserMessage {
            text,
            attachments: Vec::new(),
        });
        self.status = Some(SubAgentState::Running);
        Ok(())
    }

    /// Applies the latest turn outcome to the lifecycle state.
    fn apply_turn_outcome(&mut self, outcome: TurnOutcome) -> SubAgentState {
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
    fn status_view(&self) -> SubAgentStatus {
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
    fn build_result(&self, sub_agent_id: SubAgentId) -> SubAgentResult {
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

impl VoState for SubAgentVoState {
    async fn load_from<R: VoReader>(reader: &R) -> Result<Self, HandlerError> {
        Ok(Self {
            status: reader.get_json(K_STATUS).await?,
            parent_session: reader.get_json(K_PARENT_SESSION).await?,
            parent_sub_agent: reader.get_json(K_PARENT_SUB_AGENT).await?,
            depth: reader.get_json(K_DEPTH).await?.unwrap_or_default(),
            budget_remaining: reader
                .get_json(K_BUDGET_REMAINING)
                .await?
                .unwrap_or_default(),
            tokens_used: reader.get_json(K_TOKENS_USED).await?.unwrap_or_default(),
            result_awakeable_id: reader.get_json(K_RESULT_AWAKEABLE_ID).await?,
            task: reader.get_json(K_TASK).await?,
            tool_subset: reader.get_json(K_TOOL_SUBSET).await?.unwrap_or_default(),
            workspace_id: reader.get_json(K_WORKSPACE_ID).await?,
            user_id: reader.get_json(K_USER_ID).await?,
            model: reader.get_json(K_MODEL).await?,
            pending: reader.get_json(K_PENDING).await?.unwrap_or_default(),
            history: reader.get_json(K_HISTORY).await?.unwrap_or_default(),
            pending_approval: reader.get_json(K_PENDING_APPROVAL).await?,
            children: reader.get_json(K_CHILDREN).await?.unwrap_or_default(),
            last_turn_summary: reader.get_json(K_LAST_TURN_SUMMARY).await?,
            tools_invoked: reader.get_json(K_TOOLS_INVOKED).await?.unwrap_or_default(),
            cancel_reason: reader.get_json(K_CANCEL_REASON).await?,
        })
    }

    fn persist_into(&self, ctx: &ObjectContext<'_>) {
        set_or_clear_opt(ctx, K_STATUS, self.status.as_ref());
        set_or_clear_opt(ctx, K_PARENT_SESSION, self.parent_session.as_ref());
        set_or_clear_opt(ctx, K_PARENT_SUB_AGENT, self.parent_sub_agent.as_ref());
        set_or_clear_scalar(ctx, K_DEPTH, self.depth, 0);
        set_or_clear_scalar(ctx, K_BUDGET_REMAINING, self.budget_remaining, 0);
        set_or_clear_scalar(ctx, K_TOKENS_USED, self.tokens_used, 0);
        set_or_clear_opt(
            ctx,
            K_RESULT_AWAKEABLE_ID,
            self.result_awakeable_id.as_ref(),
        );
        set_or_clear_opt(ctx, K_TASK, self.task.as_ref());
        set_or_clear_vec(ctx, K_TOOL_SUBSET, &self.tool_subset);
        set_or_clear_opt(ctx, K_WORKSPACE_ID, self.workspace_id.as_ref());
        set_or_clear_opt(ctx, K_USER_ID, self.user_id.as_ref());
        set_or_clear_opt(ctx, K_MODEL, self.model.as_ref());
        set_or_clear_vec(ctx, K_PENDING, &self.pending);
        set_or_clear_vec(ctx, K_HISTORY, &self.history);
        set_or_clear_opt(ctx, K_PENDING_APPROVAL, self.pending_approval.as_ref());
        set_or_clear_vec(ctx, K_CHILDREN, &self.children);
        set_or_clear_opt(ctx, K_LAST_TURN_SUMMARY, self.last_turn_summary.as_ref());
        set_or_clear_scalar(ctx, K_TOOLS_INVOKED, self.tools_invoked, 0);
        set_or_clear_opt(ctx, K_CANCEL_REASON, self.cancel_reason.as_ref());
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

pub(crate) struct SubAgentTurnAdapter;

impl AgentAdapter for SubAgentTurnAdapter {
    fn children_state_key(&self) -> &'static str {
        K_CHILDREN
    }

    fn budget_state_key(&self) -> Option<&'static str> {
        Some(K_BUDGET_REMAINING)
    }

    fn sub_agent_id(&self, ctx: &ObjectContext<'_>) -> Option<SubAgentId> {
        Some(ctx.key().to_string())
    }

    async fn is_cancelled(&self, ctx: &ObjectContext<'_>) -> Result<bool, HandlerError> {
        Ok(SubAgentVoState::load_from(ctx)
            .await?
            .cancel_reason
            .is_some())
    }

    async fn has_pending_approval(&self, ctx: &ObjectContext<'_>) -> Result<bool, HandlerError> {
        Ok(SubAgentVoState::load_from(ctx)
            .await?
            .pending_approval
            .is_some())
    }

    async fn enforce_limits(&self, ctx: &ObjectContext<'_>) -> Result<(), HandlerError> {
        let state = SubAgentVoState::load_from(ctx).await?;
        if state.depth >= MAX_SUB_AGENT_DEPTH {
            return Err(TerminalError::new(format!(
                "sub-agent depth exceeds maximum ({MAX_SUB_AGENT_DEPTH})"
            ))
            .into());
        }
        Ok(())
    }

    async fn build_request(
        &self,
        ctx: &ObjectContext<'_>,
    ) -> Result<Option<CompletionRequest>, HandlerError> {
        let state = SubAgentVoState::load_from(ctx).await?;
        state.ensure_initialized().map_err(to_handler_error)?;
        if state.budget_exhausted() {
            return Ok(None);
        }

        let mut request = build_completion_request(&state)?;
        request.messages.extend(state.history.clone());
        Ok(Some(request))
    }

    async fn session_meta(&self, ctx: &ObjectContext<'_>) -> Result<SessionMeta, HandlerError> {
        synthetic_session_meta(&SubAgentVoState::load_from(ctx).await?)
    }

    async fn owning_session_id(&self, ctx: &ObjectContext<'_>) -> Result<SessionId, HandlerError> {
        SubAgentVoState::load_from(ctx)
            .await?
            .parent_session
            .ok_or_else(|| {
                TerminalError::new("sub-agent parent session missing while dispatching tool").into()
            })
    }

    async fn apply_outcome(
        &self,
        ctx: &ObjectContext<'_>,
        outcome: TurnOutcome,
    ) -> Result<(), HandlerError> {
        let mut state = SubAgentVoState::load_from(ctx).await?;
        if !matches!(
            (state.current_status(), outcome),
            (SubAgentState::Failed, TurnOutcome::Idle)
        ) {
            state.apply_turn_outcome(outcome);
        }
        state.persist_into(ctx);
        Ok(())
    }

    async fn emit_turn_budget_exceeded(
        &self,
        ctx: &ObjectContext<'_>,
        max_turns: usize,
    ) -> Result<(), HandlerError> {
        let mut state = SubAgentVoState::load_from(ctx).await?;
        let parent_session = state.parent_session;
        state.status = Some(SubAgentState::Failed);
        state.last_turn_summary = Some(format!("turn budget exceeded ({max_turns})"));
        state.persist_into(ctx);

        if let Some(parent_session) = parent_session {
            persist_parent_session_event(
                ctx,
                parent_session,
                Event::Error {
                    message: format!(
                        "sub-agent {} turn budget exceeded ({max_turns}), stopping",
                        ctx.key()
                    ),
                    recoverable: true,
                },
            )
            .await?;
        }
        Ok(())
    }

    async fn record_response(
        &self,
        ctx: &ObjectContext<'_>,
        response: &CompletionResponse,
    ) -> Result<(), HandlerError> {
        let mut state = SubAgentVoState::load_from(ctx).await?;
        let token_usage = response.token_usage();
        let token_cost = (token_usage.total_input_tokens() + token_usage.output_tokens) as u64;
        state.record_token_usage(token_cost);
        let parent_session = state.parent_session;
        state.last_turn_summary = summarize_response_text(response);
        apply_response_to_history(&mut state.history, response);
        state.persist_into(ctx);
        if let Some(parent_session) = parent_session
            && token_cost > 0
        {
            ctx.service_client::<RestateSessionStoreClient>()
                .record_segment_turn_usage(Json(RecordSegmentTurnUsageRequest {
                    session_id: parent_session,
                    token_cost,
                }))
                .send();
        }
        Ok(())
    }

    async fn current_segment(
        &self,
        ctx: &ObjectContext<'_>,
    ) -> Result<Option<ActiveSegment>, HandlerError> {
        let parent_session = self.owning_session_id(ctx).await?;
        let segment = ctx
            .service_client::<RestateSessionStoreClient>()
            .get_active_segment(Json(parent_session))
            .call()
            .await?
            .into_inner();
        Ok(segment.map(|segment| segment.active_view()))
    }

    async fn record_segment_tool_use(
        &self,
        ctx: &ObjectContext<'_>,
        tool_name: &str,
    ) -> Result<(), HandlerError> {
        ctx.service_client::<RestateSessionStoreClient>()
            .record_segment_tool_use(Json(RecordSegmentToolUseRequest {
                session_id: self.owning_session_id(ctx).await?,
                tool_name: tool_name.to_string(),
            }))
            .send();
        Ok(())
    }

    async fn record_tool_result(
        &self,
        ctx: &ObjectContext<'_>,
        tool_id: ToolCallId,
        invocation: &ToolInvocation,
        output: &ToolOutput,
    ) -> Result<(), HandlerError> {
        let mut state = SubAgentVoState::load_from(ctx).await?;
        let assistant_text = if invocation.name == "dispatch_sub_agent" {
            dispatch_history_text(output)
        } else {
            format!("Calling tool {}", invocation.name)
        };
        state.history.push(ContextMessage::assistant_tool_call(
            ToolInvocation {
                id: invocation.id.clone(),
                name: invocation.name.clone(),
                input: invocation.input.clone(),
            },
            assistant_text,
        ));
        state.history.push(ContextMessage::tool_result(
            invocation
                .id
                .clone()
                .unwrap_or_else(|| tool_id.0.to_string()),
            output.to_text(),
            Some(output.content.clone()),
        ));
        state.tools_invoked = state.tools_invoked.saturating_add(1);
        state.persist_into(ctx);
        Ok(())
    }

    async fn record_denied_tool(
        &self,
        ctx: &ObjectContext<'_>,
        tool_id: ToolCallId,
        invocation: &ToolInvocation,
        output: &ToolOutput,
    ) -> Result<(), HandlerError> {
        let mut state = SubAgentVoState::load_from(ctx).await?;
        let parent_session = state.parent_session.ok_or_else(|| {
            TerminalError::new("sub-agent parent session missing while dispatching tool")
        })?;
        persist_parent_session_event(
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
        state.history.push(ContextMessage::assistant_tool_call(
            ToolInvocation {
                id: invocation.id.clone(),
                name: invocation.name.clone(),
                input: invocation.input.clone(),
            },
            format!("Approval required for {}", invocation.name),
        ));
        state.history.push(ContextMessage::tool_result(
            invocation
                .id
                .clone()
                .unwrap_or_else(|| tool_id.0.to_string()),
            output.to_text(),
            Some(output.content.clone()),
        ));
        state.persist_into(ctx);
        Ok(())
    }

    async fn drain_pending_before_request(
        &self,
        ctx: &ObjectContext<'_>,
    ) -> Result<(), HandlerError> {
        let mut state = SubAgentVoState::load_from(ctx).await?;
        let pending = std::mem::take(&mut state.pending);
        for message in &pending {
            state
                .history
                .push(ContextMessage::user(render_user_message(message)));
        }
        state.persist_into(ctx);
        Ok(())
    }

    async fn dispatch_child(
        &self,
        ctx: &mut ObjectContext<'_>,
        input: DispatchSubAgentInput,
    ) -> Result<DispatchedSubAgent, HandlerError> {
        let state = SubAgentVoState::load_from(ctx).await?;
        let parent_session = state.parent_session.ok_or_else(|| {
            TerminalError::new("sub-agent parent session missing while dispatching tool")
        })?;
        let workspace_id = state
            .workspace_id
            .clone()
            .ok_or_else(|| TerminalError::new("sub-agent workspace_id missing"))?;
        let user_id = state
            .user_id
            .clone()
            .ok_or_else(|| TerminalError::new("sub-agent user_id missing"))?;
        let model = state
            .model
            .clone()
            .ok_or_else(|| TerminalError::new("sub-agent model missing"))?;

        dispatch_sub_agent(
            ctx,
            self.children_state_key(),
            self.budget_state_key(),
            parent_session,
            self.sub_agent_id(ctx),
            state.depth,
            input,
            workspace_id,
            user_id,
            model,
        )
        .await
    }

    async fn set_pending_approval(
        &self,
        ctx: &ObjectContext<'_>,
        awakeable_id: String,
    ) -> Result<(), HandlerError> {
        let mut state = SubAgentVoState::load_from(ctx).await?;
        state.pending_approval = Some(awakeable_id);
        state.status = Some(SubAgentState::WaitingApproval);
        state.persist_into(ctx);
        Ok(())
    }

    async fn clear_pending_approval(&self, ctx: &ObjectContext<'_>) -> Result<(), HandlerError> {
        let mut state = SubAgentVoState::load_from(ctx).await?;
        state.pending_approval = None;
        state.status = Some(SubAgentState::Running);
        state.persist_into(ctx);
        Ok(())
    }
}

impl SubAgent for SubAgentImpl {
    #[tracing::instrument(skip(self, ctx, msg))]
    async fn post_message(
        &self,
        mut ctx: ObjectContext<'_>,
        msg: Json<SubAgentMessage>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "post_message");
        let message = msg.into_inner();
        let mut state = SubAgentVoState::load_from(&ctx).await?;
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
        state.persist_into(&ctx);

        let runner = TurnRunner::new(SubAgentTurnAdapter);
        runner.run_until_idle(&mut ctx, MAX_TURNS_PER_POST).await?;
        maybe_resolve_parent_awakeable(&ctx).await
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn status(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<SubAgentStatus>, HandlerError> {
        annotate_restate_handler_span("SubAgent", "status");
        Ok(Json::from(
            SubAgentVoState::load_from(&ctx).await?.status_view(),
        ))
    }

    #[tracing::instrument(skip(self, ctx, reason))]
    async fn cancel(&self, ctx: ObjectContext<'_>, reason: String) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "cancel");
        let mut state = SubAgentVoState::load_from(&ctx).await?;
        state.cancel_reason = Some(reason.clone());
        state.status = Some(SubAgentState::Cancelled);
        let children = state.children.clone();
        state.persist_into(&ctx);

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
        Ok(Json::from(
            TurnRunner::new(SubAgentTurnAdapter)
                .run_once(&mut ctx)
                .await?,
        ))
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn destroy(&self, ctx: ObjectContext<'_>) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "destroy");
        ctx.clear_all();
        tracing::info!(key = %ctx.key(), "sub-agent VO state cleared");
        Ok(())
    }
}

async fn maybe_resolve_parent_awakeable(ctx: &ObjectContext<'_>) -> Result<(), HandlerError> {
    let mut state = SubAgentVoState::load_from(ctx).await?;
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
    state.persist_into(ctx);
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
        response_format: None,
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
    let configured = OrchestratorCtx::current().tool_schemas.clone();
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
    OrchestratorCtx::current()
        .providers
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

async fn persist_parent_session_event(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    event: Event,
) -> Result<(), HandlerError> {
    ctx.service_client::<RestateSessionStoreClient>()
        .append_event(Json(AppendEventRequest { session_id, event }))
        .call()
        .await?;
    Ok(())
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

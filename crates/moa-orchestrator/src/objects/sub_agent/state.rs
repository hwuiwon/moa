//! Durable SubAgent VO state projection.

use super::*;

pub(super) const K_STATUS: &str = "status";
pub(super) const K_PENDING: &str = "pending";
pub(super) const K_PENDING_APPROVAL: &str = "pending_approval";
pub(super) const K_CHILDREN: &str = "children";
pub(super) const K_LAST_TURN_SUMMARY: &str = "last_turn_summary";
pub(super) const K_PARENT_SESSION: &str = "parent_session";
pub(super) const K_PARENT_SUB_AGENT: &str = "parent_sub_agent";
pub(super) const K_DEPTH: &str = "depth";
pub(super) const K_BUDGET_REMAINING: &str = "budget_remaining";
pub(super) const K_TOKENS_USED: &str = "tokens_used";
pub(super) const K_RESULT_AWAKEABLE_ID: &str = "result_awakeable_id";
pub(super) const K_TASK: &str = "task";
pub(super) const K_TOOL_SUBSET: &str = "tool_subset";
pub(super) const K_WORKSPACE_ID: &str = "workspace_id";
pub(super) const K_USER_ID: &str = "user_id";
pub(super) const K_MODEL: &str = "model";
pub(super) const K_HISTORY: &str = "history";
pub(super) const K_TOOLS_INVOKED: &str = "tools_invoked";
pub(super) const K_CANCEL_REASON: &str = "cancel_reason";
pub(super) const MAX_TURNS_PER_POST: usize = 50;

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
    pub(super) fn current_status(&self) -> SubAgentState {
        self.status.unwrap_or(SubAgentState::Completed)
    }

    /// Ensures the child was initialized before handling follow-up messages or turns.
    pub(super) fn ensure_initialized(&self) -> moa_core::Result<()> {
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
    pub(super) fn enqueue_follow_up(&mut self, text: String) -> moa_core::Result<()> {
        self.ensure_initialized()?;
        self.pending.push(UserMessage {
            text,
            attachments: Vec::new(),
        });
        self.status = Some(SubAgentState::Running);
        Ok(())
    }

    /// Applies the latest turn outcome to the lifecycle state.
    pub(super) fn apply_turn_outcome(&mut self, outcome: TurnOutcome) -> SubAgentState {
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
    pub(super) fn status_view(&self) -> SubAgentStatus {
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
    pub(super) fn build_result(&self, sub_agent_id: SubAgentId) -> SubAgentResult {
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

impl SubAgentVoState {
    pub(super) fn task_hash(&self) -> String {
        crate::sub_agent_dispatch::task_hash(
            self.task.as_deref().unwrap_or_default(),
            &self.tool_subset,
        )
    }
}

#[cfg(test)]
mod tests {
    use moa_core::{ModelId, SessionId, SubAgentMessage, UserId, WorkspaceId};

    use super::SubAgentVoState;
    use moa_core::SubAgentState;

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

    #[test]
    fn task_hash_uses_shared_dispatch_hash() {
        let mut state = SubAgentVoState::default();
        state
            .initialize(&initial_task())
            .expect("initial task should seed state");

        assert_eq!(state.task_hash(), "9be010055aa996c5");
    }
}

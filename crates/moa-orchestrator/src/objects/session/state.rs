//! Durable Session VO state projection.

use super::*;
use moa_core::TurnOutcome;

pub(super) const K_META: &str = "meta";
pub(super) const K_STATUS: &str = "status";
pub(super) const K_PENDING: &str = "pending";
pub(super) const K_CHILDREN: &str = "children";
pub(super) const K_LAST_TURN_SUMMARY: &str = "last_turn_summary";
pub(super) const K_CANCEL_FLAG: &str = "cancel_flag";
pub(super) const K_CURRENT_SEGMENT: &str = "current_segment";

/// Serializable projection of the Session VO's durable state keys.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionVoState {
    /// Persisted session metadata mirror.
    pub meta: Option<SessionMeta>,
    /// Current lifecycle status held in Restate state.
    pub status: Option<SessionStatus>,
    /// Buffered user messages waiting for the next `TurnExecution` workflow.
    pub pending: Vec<UserMessage>,
    /// Placeholder for sub-agent children introduced in R08.
    pub children: Vec<SubAgentChildRef>,
    /// Human-readable stub summary of the last drained turn.
    pub last_turn_summary: Option<String>,
    /// Cooperative cancellation flag checked at turn boundaries.
    pub cancel_flag: Option<CancelMode>,
    /// Active task segment, when one has been created for the session.
    pub current_segment: Option<ActiveSegment>,
}

impl SessionVoState {
    /// Initializes the projection from persisted session metadata.
    pub fn set_meta(&mut self, meta: SessionMeta) {
        self.status = Some(meta.status.clone());
        self.meta = Some(meta);
    }

    /// Returns the current lifecycle status, defaulting to `Created` when state is empty.
    pub fn current_status(&self) -> SessionStatus {
        self.status.clone().unwrap_or(SessionStatus::Created)
    }

    /// Ensures that session metadata has been initialized before mutations proceed.
    pub fn ensure_initialized(&self) -> MoaResult<&SessionMeta> {
        self.meta.as_ref().ok_or_else(|| {
            MoaError::ValidationError(
                "Session metadata missing. Initialize the VO via SessionStore/init_session_vo first."
                    .to_string(),
            )
        })
    }

    /// Queues one user message and transitions the session into `Running`.
    pub fn enqueue_message(&mut self, msg: UserMessage, now: DateTime<Utc>) -> MoaResult<()> {
        self.ensure_initialized()?;
        self.pending.push(msg);
        self.set_status(SessionStatus::Running, now);
        Ok(())
    }

    /// Applies a turn outcome to the lifecycle state.
    ///
    /// In the existing MOA status model, an idle turn parks the session in `Paused`.
    pub fn apply_turn_outcome(
        &mut self,
        outcome: TurnOutcome,
        now: DateTime<Utc>,
    ) -> SessionStatus {
        let next_status = match outcome {
            TurnOutcome::Continue => SessionStatus::Running,
            TurnOutcome::Idle => SessionStatus::Paused,
            TurnOutcome::Cancelled => SessionStatus::Cancelled,
        };
        self.set_status(next_status.clone(), now);
        next_status
    }

    /// Records a cooperative cancellation request.
    pub fn set_cancel_flag(&mut self, mode: CancelMode) {
        self.cancel_flag = Some(mode);
    }

    /// Consumes the current cancellation flag, if any.
    pub fn take_cancel_flag(&mut self) -> Option<CancelMode> {
        self.cancel_flag.take()
    }

    /// Drains buffered user messages and records a short stub summary.
    pub fn drain_pending_messages(&mut self) -> usize {
        let drained = self.pending.len();
        self.pending.clear();
        self.last_turn_summary = if drained == 0 {
            None
        } else if drained == 1 {
            Some("drained 1 queued message".to_string())
        } else {
            Some(format!("drained {drained} queued messages"))
        };
        drained
    }

    /// Clears the in-memory projection back to an empty VO.
    pub fn destroy(&mut self) {
        *self = Self::default();
    }

    /// Replaces the active task segment.
    pub fn set_current_segment(&mut self, segment: ActiveSegment) {
        self.current_segment = Some(segment);
    }

    /// Records a tool usage on the active task segment.
    pub fn record_segment_tool_use(&mut self, tool_name: &str) {
        let Some(segment) = self.current_segment.as_mut() else {
            return;
        };
        if !segment.tools_used.iter().any(|tool| tool == tool_name) {
            segment.tools_used.push(tool_name.to_string());
        }
    }

    /// Records one completed model turn on the active task segment.
    pub fn record_segment_turn_usage(&mut self, token_cost: u64) {
        let Some(segment) = self.current_segment.as_mut() else {
            return;
        };
        segment.turn_count = segment.turn_count.saturating_add(1);
        segment.token_cost = segment.token_cost.saturating_add(token_cost);
    }

    /// Adds a root-owned child sub-agent reference if it is not already registered.
    pub fn register_child(&mut self, child: SubAgentChildRef) -> bool {
        if self.children.iter().any(|existing| existing.id == child.id) {
            return false;
        }
        self.children.push(child);
        true
    }

    /// Caches a terminal child result until the parent consumes it.
    pub fn mark_child_terminal(&mut self, input: MarkSubAgentChildTerminalInput) -> bool {
        let Some(child) = self
            .children
            .iter_mut()
            .find(|child| child.id == input.sub_agent_id)
        else {
            return false;
        };
        if child.terminal.is_some() {
            return false;
        }
        child.terminal = Some(input.terminal);
        true
    }

    /// Removes and returns a cached terminal child result.
    pub fn consume_child_terminal(&mut self, sub_agent_id: &str) -> Option<SubAgentTerminalResult> {
        let index = self
            .children
            .iter()
            .position(|child| child.id == sub_agent_id && child.terminal.is_some())?;
        self.children.remove(index).terminal
    }

    /// Removes a root-owned child sub-agent reference by id.
    pub fn remove_child(&mut self, sub_agent_id: &str) -> bool {
        let before = self.children.len();
        self.children.retain(|child| child.id != sub_agent_id);
        self.children.len() != before
    }

    /// Returns whether the session currently owns the child sub-agent id.
    #[must_use]
    pub fn owns_child(&self, sub_agent_id: &str) -> bool {
        self.children.iter().any(|child| child.id == sub_agent_id)
    }

    pub(super) fn set_status(&mut self, status: SessionStatus, now: DateTime<Utc>) {
        self.status = Some(status.clone());
        if let Some(meta) = self.meta.as_mut() {
            meta.status = status.clone();
            meta.updated_at = now;
            if matches!(
                status,
                SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed
            ) && meta.completed_at.is_none()
            {
                meta.completed_at = Some(now);
            }
        }
    }
}

impl VoState for SessionVoState {
    async fn load_from<R: VoReader>(reader: &R) -> Result<Self, HandlerError> {
        Ok(Self {
            meta: reader.get_json(K_META).await?,
            status: reader.get_json(K_STATUS).await?,
            pending: reader.get_json(K_PENDING).await?.unwrap_or_default(),
            children: reader.get_json(K_CHILDREN).await?.unwrap_or_default(),
            last_turn_summary: reader.get_json(K_LAST_TURN_SUMMARY).await?,
            cancel_flag: reader.get_json(K_CANCEL_FLAG).await?,
            current_segment: reader.get_json(K_CURRENT_SEGMENT).await?,
        })
    }

    fn persist_into(&self, ctx: &ObjectContext<'_>) {
        set_or_clear_opt(ctx, K_META, self.meta.as_ref());
        set_or_clear_opt(ctx, K_STATUS, self.status.as_ref());
        set_or_clear_vec(ctx, K_PENDING, &self.pending);
        set_or_clear_vec(ctx, K_CHILDREN, &self.children);
        set_or_clear_opt(ctx, K_LAST_TURN_SUMMARY, self.last_turn_summary.as_ref());
        set_or_clear_opt(ctx, K_CANCEL_FLAG, self.cancel_flag.as_ref());
        set_or_clear_opt(ctx, K_CURRENT_SEGMENT, self.current_segment.as_ref());
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::{Attachment, Channel, ModelId};

    use super::SessionVoState;
    use moa_core::TurnOutcome;

    fn test_message(text: &str) -> moa_core::UserMessage {
        moa_core::UserMessage {
            text: text.to_string(),
            attachments: vec![Attachment {
                id: None,
                name: "a.txt".to_string(),
                mime_type: Some("text/plain".to_string()),
                sha256: None,
                url: None,
                path: None,
                size_bytes: Some(3),
            }],
        }
    }

    fn test_meta() -> moa_core::SessionMeta {
        moa_core::SessionMeta {
            tenant_id: moa_core::TenantId::new(),
            channel: Channel::Chat,
            model: ModelId::new("test-model"),
            ..moa_core::SessionMeta::default()
        }
    }

    #[test]
    fn session_vo_requires_meta_before_enqueue() {
        let mut state = SessionVoState::default();
        let error = state
            .enqueue_message(test_message("hello"), Utc::now())
            .expect_err("enqueue should fail without metadata");

        assert!(error.to_string().contains("Session metadata missing"));
    }

    #[test]
    fn session_vo_queues_messages_and_transitions_to_running() {
        let mut state = SessionVoState::default();
        state.set_meta(test_meta());
        state
            .enqueue_message(test_message("hello"), Utc::now())
            .expect("enqueue should succeed");

        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.current_status(), moa_core::SessionStatus::Running);
    }

    #[test]
    fn session_vo_idle_turn_maps_to_paused_status() {
        let mut state = SessionVoState::default();
        state.set_meta(test_meta());
        let status = state.apply_turn_outcome(TurnOutcome::Idle, Utc::now());

        assert_eq!(status, moa_core::SessionStatus::Paused);
        assert_eq!(state.current_status(), moa_core::SessionStatus::Paused);
    }

    #[test]
    fn session_vo_cancel_flag_round_trips() {
        let mut state = SessionVoState::default();
        state.set_cancel_flag(moa_core::CancelMode::Soft);

        assert_eq!(state.take_cancel_flag(), Some(moa_core::CancelMode::Soft));
        assert_eq!(state.take_cancel_flag(), None);
    }

    #[test]
    fn session_vo_destroy_clears_projection() {
        let mut state = SessionVoState::default();
        state.set_meta(test_meta());
        state
            .enqueue_message(test_message("hello"), Utc::now())
            .expect("enqueue should succeed");
        state.children.push(moa_core::SubAgentChildRef {
            id: "child-1".to_string(),
            task_hash: "hash-1".to_string(),
            budget_tokens: 0,
            terminal: None,
        });
        state.last_turn_summary = Some("summary".to_string());
        state.set_cancel_flag(moa_core::CancelMode::Hard);
        state.destroy();

        assert_eq!(state, SessionVoState::default());
    }

    #[test]
    fn session_child_registry_is_idempotent_by_child_id() {
        // Pins: root delegation registration preserves one active child ref per id.
        let mut state = SessionVoState::default();
        let child = moa_core::SubAgentChildRef {
            id: "child-1".to_string(),
            task_hash: "hash-1".to_string(),
            budget_tokens: 128,
            terminal: None,
        };

        assert!(state.register_child(child.clone()));
        assert!(!state.register_child(child));
        assert_eq!(state.children.len(), 1);
        assert!(state.owns_child("child-1"));
    }

    #[test]
    fn session_child_registry_remove_is_exact() {
        // Pins: root delegation cleanup removes only the requested active child ref.
        let mut state = SessionVoState::default();
        state.register_child(moa_core::SubAgentChildRef {
            id: "child-1".to_string(),
            task_hash: "hash-1".to_string(),
            budget_tokens: 128,
            terminal: None,
        });
        state.register_child(moa_core::SubAgentChildRef {
            id: "child-2".to_string(),
            task_hash: "hash-2".to_string(),
            budget_tokens: 256,
            terminal: None,
        });

        assert!(state.remove_child("child-1"));
        assert!(!state.remove_child("missing"));
        assert_eq!(
            state.children,
            vec![moa_core::SubAgentChildRef {
                id: "child-2".to_string(),
                task_hash: "hash-2".to_string(),
                budget_tokens: 256,
                terminal: None,
            }]
        );
    }

    #[test]
    fn session_child_terminal_result_is_consumed_once() {
        // Pins: root wait consumes a cached terminal child result exactly once.
        let mut state = SessionVoState::default();
        state.register_child(moa_core::SubAgentChildRef {
            id: "child-1".to_string(),
            task_hash: "hash-1".to_string(),
            budget_tokens: 128,
            terminal: None,
        });
        let terminal = moa_core::SubAgentTerminalResult {
            state: moa_core::SubAgentState::Completed,
            result: moa_core::SubAgentResult {
                sub_agent_id: "child-1".to_string(),
                success: true,
                output: "done".to_string(),
                tokens_used: 17,
                tools_invoked: 2,
                error: None,
            },
        };

        assert!(
            state.mark_child_terminal(moa_core::MarkSubAgentChildTerminalInput {
                sub_agent_id: "child-1".to_string(),
                terminal: terminal.clone(),
            })
        );
        assert!(
            !state.mark_child_terminal(moa_core::MarkSubAgentChildTerminalInput {
                sub_agent_id: "child-1".to_string(),
                terminal: terminal.clone(),
            })
        );
        assert_eq!(state.consume_child_terminal("child-1"), Some(terminal));
        assert_eq!(state.consume_child_terminal("child-1"), None);
        assert!(!state.owns_child("child-1"));
    }
}

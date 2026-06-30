//! Durable Session VO state projection.

use super::*;
use moa_core::traits::Identity;
use moa_core::{
    AgentSignalId, ChildSignalKind, ParentResumePolicy, TurnOutcome, UnreadChildSignal,
    WorkerSignal,
};

pub(super) const K_META: &str = "meta";
pub(super) const K_STATUS: &str = "status";
pub(super) const K_PENDING: &str = "pending";
pub(super) const K_CHILDREN: &str = "children";
pub(super) const K_LAST_TURN_SUMMARY: &str = "last_turn_summary";
pub(super) const K_CANCEL_FLAG: &str = "cancel_flag";
pub(super) const K_CURRENT_SEGMENT: &str = "current_segment";
pub(super) const K_NARRATION_TICK_GENERATION: &str = "narration_tick_generation";
pub(super) const K_NARRATION_TICK_OUTSTANDING: &str = "narration_tick_outstanding";
pub(super) const K_NARRATION_SEQ: &str = "narration_seq";
pub(super) const K_LAST_NARRATED_MARKER: &str = "last_narrated_marker";
pub(super) const K_LAST_NARRATION_AT: &str = "last_narration_at";
pub(super) const K_NARRATION_WINDOW_START: &str = "narration_window_start";
pub(super) const K_NARRATION_WINDOW_COUNT: &str = "narration_window_count";
pub(super) const K_OWNING_IDENTITY: &str = "owning_identity";
pub(super) const K_UNREAD_CHILD_SIGNALS: &str = "unread_child_signals";
pub(super) const K_PENDING_PARENT_RESUME_SIGNAL: &str = "pending_parent_resume_signal";
pub(super) const K_RESUME_BUDGET: &str = "resume_budget";
pub(super) const K_RESUME_TURN: &str = "resume_turn";
pub(super) const K_CHILD_LIVENESS_GENERATION: &str = "child_liveness_generation";
pub(super) const K_CHILD_LIVENESS: &str = "child_liveness";

/// Maximum unread child→parent control-plane signals retained on the coordinator VO.
///
/// Kept small so the control-plane projection never bloats parent state. When the cap
/// is exceeded, action-required kinds (`NeedsInput`/`Blocked`) are preferentially kept
/// over informational `Finding`s during eviction.
pub(super) const MAX_UNREAD_CHILD_SIGNALS: usize = 32;

/// Per-session guarded-resume budget: a rolling window start and the resume count
/// dispatched within it.
///
/// Persisted with the Session VO. The rolling-window cap and length are sourced from
/// `MoaConfig` session limits (`worker_resume_max_per_window` /
/// `worker_resume_window_ms`); the budget is checked by the resume-eligibility gate
/// and consumed only on an actual dispatch.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResumeBudget {
    /// Start of the current rolling resume window, if one has opened.
    pub window_start: Option<DateTime<Utc>>,
    /// Number of guarded resumes dispatched in the current window.
    pub count: u32,
}

impl ResumeBudget {
    /// Returns whether another guarded resume may be dispatched at `now`.
    ///
    /// An elapsed (or never-opened) window resets the accounting, so the cap only binds
    /// within one rolling `window_ms`. `max == 0` disables resume entirely.
    #[must_use]
    pub fn allows(&self, now: DateTime<Utc>, window_ms: u64, max: u32) -> bool {
        if max == 0 {
            return false;
        }
        match self.window_start {
            Some(start)
                if now.signed_duration_since(start)
                    < chrono::Duration::milliseconds(window_ms as i64) =>
            {
                self.count < max
            }
            // Fresh or elapsed window: the next dispatch opens a new window.
            _ => true,
        }
    }

    /// Records one dispatched resume at `now`, resetting the window when it has elapsed.
    pub fn consume(&mut self, now: DateTime<Utc>, window_ms: u64) {
        match self.window_start {
            Some(start)
                if now.signed_duration_since(start)
                    < chrono::Duration::milliseconds(window_ms as i64) =>
            {
                self.count = self.count.saturating_add(1);
            }
            _ => {
                self.window_start = Some(now);
                self.count = 1;
            }
        }
    }
}

/// Dispatch-time context for an in-flight guarded coordinator resume turn.
///
/// Records which turn was dispatched for a resume and the snapshot of unread signal ids
/// folded into its instruction, so [`SessionVoState::clear_resume_on_outcome`] consumes
/// exactly that snapshot when the turn completes (signals that arrive mid-turn stay
/// queued for the next resume).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResumeTurnContext {
    /// Turn id dispatched for the guarded resume.
    pub turn_id: String,
    /// Unread signal ids consumed by this resume turn at dispatch time.
    pub consumed_signal_ids: Vec<AgentSignalId>,
}

/// Per-active-child liveness-watchdog scheduling state held on the Session VO.
///
/// One entry exists while a per-child `check_child_liveness` delayed self-call is
/// outstanding. `generation` is drawn from the session-wide monotonic
/// [`SessionVoState::child_liveness_generation`] counter so a tick scheduled by a
/// superseded arming is recognized as stale and ignored when it fires.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChildLivenessState {
    /// Child worker this watchdog entry tracks.
    pub worker_id: WorkerId,
    /// Scheduling generation of the currently outstanding liveness check.
    pub generation: u64,
}

/// Serializable projection of the Session VO's durable state keys.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionVoState {
    /// Persisted session metadata mirror.
    pub meta: Option<SessionMeta>,
    /// Current lifecycle status held in Restate state.
    pub status: Option<SessionStatus>,
    /// Buffered user messages waiting for the next `TurnExecution` workflow.
    pub pending: Vec<UserMessage>,
    /// Placeholder for worker children introduced in R08.
    pub children: Vec<WorkerChildRef>,
    /// Human-readable stub summary of the last drained turn.
    pub last_turn_summary: Option<String>,
    /// Requested cancellation scope, recorded at the most recent cancel request.
    pub cancel_flag: Option<CancelScope>,
    /// Active task segment, when one has been created for the session.
    pub current_segment: Option<ActiveSegment>,
    /// Current progress-narration scheduling generation. Bumped on each active-edge
    /// (re)start so a delayed tick scheduled by a superseded generation is ignored.
    pub narration_tick_generation: u64,
    /// Whether a narration tick is scheduled and not yet stopped. Guarantees a single
    /// outstanding tick so `register_child`/turn-start edges cannot fan out overlapping ticks.
    pub narration_tick_outstanding: bool,
    /// Monotonic narration sequence used to build the `narration:{session}:{seq}` dedupe key.
    pub narration_seq: u64,
    /// Change cursor (semantic marker) of the most recently narrated active sources.
    pub last_narrated_marker: Option<String>,
    /// Journaled instant of the most recent narration dispatch, for the interval gate.
    pub last_narration_at: Option<DateTime<Utc>>,
    /// Rolling narration window start, for the per-window cost cap.
    pub narration_window_start: Option<DateTime<Utc>>,
    /// Narrations dispatched in the current rolling window.
    pub narration_window_count: u32,
    /// Owning participant identity captured for self-originated narration reads. Sourced
    /// from the first verified turn participant, falling back to session metadata.
    pub owning_identity: Option<Identity>,
    /// Recent unread child→parent control-plane signals, capped to a small window.
    ///
    /// Stores signal CONTENT (kind/summary/input request) so a Task-6 resume/drain turn
    /// can compile it into the coordinator prompt without re-reading the event log.
    /// Eviction prefers to keep action-required kinds (`NeedsInput`/`Blocked`).
    pub unread_child_signals: Vec<UnreadChildSignal>,
    /// Signal armed for a guarded coordinator auto-resume, when one is pending.
    ///
    /// Set by the resume-eligibility gate (decision only). The actual resume-turn
    /// dispatch and clearing on completion are wired in Task 6.
    pub pending_parent_resume_signal: Option<AgentSignalId>,
    /// Per-session guarded-resume budget, consumed on each guarded-resume dispatch.
    pub resume_budget: ResumeBudget,
    /// In-flight guarded coordinator resume turn and its dispatch-time unread snapshot,
    /// drained on `record_turn_outcome` when that turn completes.
    pub resume_turn: Option<ResumeTurnContext>,
    /// Session-wide monotonic counter minting one liveness-check generation per arming.
    ///
    /// Monotonic so a re-armed (or re-registered) child never reuses a prior
    /// generation, making any stray in-flight tick from before a clear/re-arm
    /// recognizable as stale.
    pub child_liveness_generation: u64,
    /// Per-child outstanding liveness-watchdog checks (single-outstanding per child).
    pub child_liveness: Vec<ChildLivenessState>,
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

    /// Records the requested cancellation scope.
    pub fn set_cancel_flag(&mut self, scope: CancelScope) {
        self.cancel_flag = Some(scope);
    }

    /// Consumes the current cancellation scope, if any.
    pub fn take_cancel_flag(&mut self) -> Option<CancelScope> {
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

    /// Adds a root-owned child worker reference if it is not already registered.
    pub fn register_child(&mut self, child: WorkerChildRef) -> bool {
        if self.children.iter().any(|existing| existing.id == child.id) {
            return false;
        }
        self.children.push(child);
        true
    }

    /// Caches a terminal child result until the parent consumes it.
    pub fn mark_child_terminal(&mut self, input: MarkWorkerChildTerminalInput) -> bool {
        let Some(child) = self
            .children
            .iter_mut()
            .find(|child| child.id == input.worker_id)
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
    pub fn consume_child_terminal(&mut self, worker_id: &str) -> Option<WorkerTerminalResult> {
        let index = self
            .children
            .iter()
            .position(|child| child.id == worker_id && child.terminal.is_some())?;
        self.children.remove(index).terminal
    }

    /// Removes a root-owned child worker reference by id.
    pub fn remove_child(&mut self, worker_id: &str) -> bool {
        let before = self.children.len();
        self.children.retain(|child| child.id != worker_id);
        // Drop any outstanding liveness watchdog for the now-removed child.
        self.clear_child_liveness(worker_id);
        self.children.len() != before
    }

    /// Returns whether the session currently owns the child worker id.
    #[must_use]
    pub fn owns_child(&self, worker_id: &str) -> bool {
        self.children.iter().any(|child| child.id == worker_id)
    }

    /// Pushes one unread child→parent control-plane signal onto the recent window.
    ///
    /// Deduplicates by `signal_id` (a retried delivery is a no-op) and caps the window
    /// to [`MAX_UNREAD_CHILD_SIGNALS`]. When evicting, an action-required signal
    /// (`NeedsInput`/`Blocked`) is preferentially kept over informational kinds: the
    /// oldest non-action-required entry is dropped first, falling back to the oldest
    /// entry only when every retained signal is action-required. Returns whether a new
    /// entry was inserted.
    pub fn push_unread_child_signal(&mut self, signal: UnreadChildSignal) -> bool {
        if self
            .unread_child_signals
            .iter()
            .any(|existing| existing.signal_id == signal.signal_id)
        {
            return false;
        }
        self.unread_child_signals.push(signal);
        while self.unread_child_signals.len() > MAX_UNREAD_CHILD_SIGNALS {
            let victim = self
                .unread_child_signals
                .iter()
                .position(|existing| !signal_kind_is_action_required(existing.kind))
                .unwrap_or(0);
            self.unread_child_signals.remove(victim);
        }
        true
    }

    /// Computes the guarded parent-resume decision for one recorded signal and arms it.
    ///
    /// Sets [`Self::pending_parent_resume_signal`] and returns `true` only when the
    /// signal opts into idle-wake (`resume_policy == IfIdle`), its kind is
    /// resume-eligible, the coordinator has no active root turn, and the rolling
    /// per-window resume budget (cap `max_per_window`, length `window_ms`) allows another
    /// resume at `now`. The budget is consumed separately, only on an actual dispatch
    /// ([`Self::record_resume_dispatch`]), so a retried delivery does not double-count.
    pub fn maybe_arm_parent_resume(
        &mut self,
        signal: &WorkerSignal,
        active_turn_id: Option<&str>,
        now: DateTime<Utc>,
        max_per_window: u32,
        window_ms: u64,
    ) -> bool {
        let eligible = matches!(signal.resume_policy, ParentResumePolicy::IfIdle)
            && signal_kind_is_resume_eligible(signal.kind)
            && active_turn_id.is_none()
            && self.resume_budget.allows(now, window_ms, max_per_window);
        if eligible {
            self.pending_parent_resume_signal = Some(signal.signal_id);
        }
        eligible
    }

    /// Records a dispatched guarded-resume turn: consumes one unit of resume budget and
    /// snapshots the current unread signal ids consumed by the turn.
    ///
    /// The snapshot is exactly the set of unread signals folded into the resume turn's
    /// instruction; [`Self::clear_resume_on_outcome`] removes only this set on completion
    /// so signals that arrive mid-turn remain queued for the next resume.
    pub fn record_resume_dispatch(&mut self, turn_id: String, now: DateTime<Utc>, window_ms: u64) {
        self.resume_budget.consume(now, window_ms);
        self.resume_turn = Some(ResumeTurnContext {
            turn_id,
            consumed_signal_ids: self
                .unread_child_signals
                .iter()
                .map(|signal| signal.signal_id)
                .collect(),
        });
    }

    /// Clears resume bookkeeping when the completing turn was the guarded-resume turn.
    ///
    /// Drains exactly the dispatch-time unread snapshot (leaving mid-turn arrivals
    /// queued) and clears `pending_parent_resume_signal`. Returns whether the completing
    /// turn matched the in-flight resume turn.
    pub fn clear_resume_on_outcome(&mut self, completed_turn_id: &str) -> bool {
        let Some(resume_turn) = self.resume_turn.as_ref() else {
            return false;
        };
        if resume_turn.turn_id != completed_turn_id {
            return false;
        }
        let consumed = self.resume_turn.take().map(|turn| turn.consumed_signal_ids);
        if let Some(consumed) = consumed {
            self.unread_child_signals
                .retain(|signal| !consumed.contains(&signal.signal_id));
        }
        self.pending_parent_resume_signal = None;
        true
    }

    /// Arms a single-outstanding liveness check for one active child.
    ///
    /// Returns the new monotonic generation to schedule with when a check is newly
    /// armed, or `None` when one is already outstanding for the child (so overlapping
    /// active edges cannot fan out multiple checks). The generation is drawn from the
    /// session-wide monotonic counter so it never collides with a superseded arming.
    pub fn arm_child_liveness(&mut self, worker_id: &str) -> Option<u64> {
        if self
            .child_liveness
            .iter()
            .any(|entry| entry.worker_id == worker_id)
        {
            return None;
        }
        self.child_liveness_generation = self.child_liveness_generation.wrapping_add(1);
        let generation = self.child_liveness_generation;
        self.child_liveness.push(ChildLivenessState {
            worker_id: worker_id.to_string(),
            generation,
        });
        Some(generation)
    }

    /// Returns whether a fired liveness check still owns scheduling for its child.
    ///
    /// A check is live only when an entry for the child is outstanding and its
    /// generation matches; a superseded or cleared check no-ops.
    #[must_use]
    pub fn liveness_generation_matches(&self, worker_id: &str, generation: u64) -> bool {
        self.child_liveness
            .iter()
            .any(|entry| entry.worker_id == worker_id && entry.generation == generation)
    }

    /// Clears the outstanding liveness check for one child (terminal/stale/removed).
    ///
    /// Removing the entry is safe because re-arming draws a fresh generation from the
    /// monotonic counter, so any stray in-flight tick can never match the re-armed child.
    pub fn clear_child_liveness(&mut self, worker_id: &str) {
        self.child_liveness
            .retain(|entry| entry.worker_id != worker_id);
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

/// Whether a signal kind must be preserved over informational kinds during unread-cap
/// eviction. Action-required kinds block the child until the coordinator responds.
#[must_use]
fn signal_kind_is_action_required(kind: ChildSignalKind) -> bool {
    matches!(kind, ChildSignalKind::NeedsInput | ChildSignalKind::Blocked)
}

/// Whether a signal kind is eligible to wake an idle coordinator (resume-eligible).
///
/// Conservative by design: only blocking/attention-or-failure kinds qualify; plain
/// `Finding`s never trigger a resume.
#[must_use]
fn signal_kind_is_resume_eligible(kind: ChildSignalKind) -> bool {
    matches!(
        kind,
        ChildSignalKind::Blocked
            | ChildSignalKind::NeedsInput
            | ChildSignalKind::Failed
            | ChildSignalKind::HeartbeatStale
    )
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
            narration_tick_generation: reader
                .get_json(K_NARRATION_TICK_GENERATION)
                .await?
                .unwrap_or_default(),
            narration_tick_outstanding: reader
                .get_json(K_NARRATION_TICK_OUTSTANDING)
                .await?
                .unwrap_or_default(),
            narration_seq: reader.get_json(K_NARRATION_SEQ).await?.unwrap_or_default(),
            last_narrated_marker: reader.get_json(K_LAST_NARRATED_MARKER).await?,
            last_narration_at: reader.get_json(K_LAST_NARRATION_AT).await?,
            narration_window_start: reader.get_json(K_NARRATION_WINDOW_START).await?,
            narration_window_count: reader
                .get_json(K_NARRATION_WINDOW_COUNT)
                .await?
                .unwrap_or_default(),
            owning_identity: reader.get_json(K_OWNING_IDENTITY).await?,
            unread_child_signals: reader
                .get_json(K_UNREAD_CHILD_SIGNALS)
                .await?
                .unwrap_or_default(),
            pending_parent_resume_signal: reader.get_json(K_PENDING_PARENT_RESUME_SIGNAL).await?,
            resume_budget: reader.get_json(K_RESUME_BUDGET).await?.unwrap_or_default(),
            resume_turn: reader.get_json(K_RESUME_TURN).await?,
            child_liveness_generation: reader
                .get_json(K_CHILD_LIVENESS_GENERATION)
                .await?
                .unwrap_or_default(),
            child_liveness: reader.get_json(K_CHILD_LIVENESS).await?.unwrap_or_default(),
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
        set_or_clear_scalar(
            ctx,
            K_NARRATION_TICK_GENERATION,
            self.narration_tick_generation,
            0,
        );
        set_or_clear_scalar(
            ctx,
            K_NARRATION_TICK_OUTSTANDING,
            self.narration_tick_outstanding,
            false,
        );
        set_or_clear_scalar(ctx, K_NARRATION_SEQ, self.narration_seq, 0);
        set_or_clear_opt(
            ctx,
            K_LAST_NARRATED_MARKER,
            self.last_narrated_marker.as_ref(),
        );
        set_or_clear_opt(ctx, K_LAST_NARRATION_AT, self.last_narration_at.as_ref());
        set_or_clear_opt(
            ctx,
            K_NARRATION_WINDOW_START,
            self.narration_window_start.as_ref(),
        );
        set_or_clear_scalar(
            ctx,
            K_NARRATION_WINDOW_COUNT,
            self.narration_window_count,
            0,
        );
        set_or_clear_opt(ctx, K_OWNING_IDENTITY, self.owning_identity.as_ref());
        set_or_clear_vec(ctx, K_UNREAD_CHILD_SIGNALS, &self.unread_child_signals);
        set_or_clear_opt(
            ctx,
            K_PENDING_PARENT_RESUME_SIGNAL,
            self.pending_parent_resume_signal.as_ref(),
        );
        set_or_clear_scalar(
            ctx,
            K_RESUME_BUDGET,
            self.resume_budget.clone(),
            ResumeBudget::default(),
        );
        set_or_clear_opt(ctx, K_RESUME_TURN, self.resume_turn.as_ref());
        set_or_clear_scalar(
            ctx,
            K_CHILD_LIVENESS_GENERATION,
            self.child_liveness_generation,
            0,
        );
        set_or_clear_vec(ctx, K_CHILD_LIVENESS, &self.child_liveness);
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
        state.set_cancel_flag(moa_core::CancelScope::CoordinatorOnly);

        assert_eq!(
            state.take_cancel_flag(),
            Some(moa_core::CancelScope::CoordinatorOnly)
        );
        assert_eq!(state.take_cancel_flag(), None);
    }

    #[test]
    fn session_vo_destroy_clears_projection() {
        let mut state = SessionVoState::default();
        state.set_meta(test_meta());
        state
            .enqueue_message(test_message("hello"), Utc::now())
            .expect("enqueue should succeed");
        state.children.push(moa_core::WorkerChildRef {
            id: "child-1".to_string(),
            task_hash: "hash-1".to_string(),
            budget_tokens: 0,
            terminal: None,
        });
        state.last_turn_summary = Some("summary".to_string());
        state.set_cancel_flag(moa_core::CancelScope::TaskTree);
        state.destroy();

        assert_eq!(state, SessionVoState::default());
    }

    #[test]
    fn session_child_registry_is_idempotent_by_child_id() {
        // Pins: root delegation registration preserves one active child ref per id.
        let mut state = SessionVoState::default();
        let child = moa_core::WorkerChildRef {
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
        state.register_child(moa_core::WorkerChildRef {
            id: "child-1".to_string(),
            task_hash: "hash-1".to_string(),
            budget_tokens: 128,
            terminal: None,
        });
        state.register_child(moa_core::WorkerChildRef {
            id: "child-2".to_string(),
            task_hash: "hash-2".to_string(),
            budget_tokens: 256,
            terminal: None,
        });

        assert!(state.remove_child("child-1"));
        assert!(!state.remove_child("missing"));
        assert_eq!(
            state.children,
            vec![moa_core::WorkerChildRef {
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
        state.register_child(moa_core::WorkerChildRef {
            id: "child-1".to_string(),
            task_hash: "hash-1".to_string(),
            budget_tokens: 128,
            terminal: None,
        });
        let terminal = moa_core::WorkerTerminalResult {
            state: moa_core::WorkerState::Completed,
            result: moa_core::WorkerResult {
                worker_id: "child-1".to_string(),
                success: true,
                output: "done".to_string(),
                tokens_used: 17,
                tools_invoked: 2,
                error: None,
            },
        };

        assert!(
            state.mark_child_terminal(moa_core::MarkWorkerChildTerminalInput {
                worker_id: "child-1".to_string(),
                terminal: terminal.clone(),
            })
        );
        assert!(
            !state.mark_child_terminal(moa_core::MarkWorkerChildTerminalInput {
                worker_id: "child-1".to_string(),
                terminal: terminal.clone(),
            })
        );
        assert_eq!(state.consume_child_terminal("child-1"), Some(terminal));
        assert_eq!(state.consume_child_terminal("child-1"), None);
        assert!(!state.owns_child("child-1"));
    }

    fn unread_entry(
        signal_id: moa_core::AgentSignalId,
        kind: moa_core::ChildSignalKind,
    ) -> moa_core::UnreadChildSignal {
        moa_core::UnreadChildSignal {
            signal_id,
            worker_id: "child".to_string(),
            kind,
            summary: "summary".to_string(),
            input_request_id: None,
            input_audience: None,
        }
    }

    fn resume_signal(
        kind: moa_core::ChildSignalKind,
        resume_policy: moa_core::ParentResumePolicy,
    ) -> moa_core::WorkerSignal {
        moa_core::WorkerSignal {
            signal_id: moa_core::AgentSignalId::new(),
            worker_id: "child".to_string(),
            parent_session: moa_core::SessionId::new(),
            parent_worker: None,
            kind,
            severity: moa_core::SignalSeverity::Warning,
            summary: "needs attention".to_string(),
            payload: serde_json::Value::Null,
            created_at: Utc::now(),
            resume_policy,
            input_request_id: None,
            input_audience: None,
        }
    }

    #[test]
    fn unread_child_signal_push_is_idempotent_by_signal_id() {
        // Pins: a retried child-signal delivery records exactly one unread entry.
        let mut state = SessionVoState::default();
        let signal_id = moa_core::AgentSignalId::new();
        let entry = unread_entry(signal_id, moa_core::ChildSignalKind::Finding);

        assert!(state.push_unread_child_signal(entry.clone()));
        assert!(!state.push_unread_child_signal(entry));
        assert_eq!(state.unread_child_signals.len(), 1);
    }

    #[test]
    fn unread_child_signal_cap_evicts_findings_before_action_required() {
        // Pins: when the unread window overflows, NeedsInput/Blocked are preserved while
        // informational Findings are evicted first.
        use moa_core::ChildSignalKind;
        let mut state = SessionVoState::default();

        let blocked_id = moa_core::AgentSignalId::new();
        assert!(state.push_unread_child_signal(unread_entry(blocked_id, ChildSignalKind::Blocked)));
        let needs_input_id = moa_core::AgentSignalId::new();
        assert!(
            state.push_unread_child_signal(unread_entry(
                needs_input_id,
                ChildSignalKind::NeedsInput,
            ))
        );
        for _ in 0..super::MAX_UNREAD_CHILD_SIGNALS + 5 {
            state.push_unread_child_signal(unread_entry(
                moa_core::AgentSignalId::new(),
                ChildSignalKind::Finding,
            ));
        }

        assert_eq!(
            state.unread_child_signals.len(),
            super::MAX_UNREAD_CHILD_SIGNALS
        );
        assert!(
            state
                .unread_child_signals
                .iter()
                .any(|signal| signal.signal_id == blocked_id),
            "Blocked signal must be preserved over evicted Findings"
        );
        assert!(
            state
                .unread_child_signals
                .iter()
                .any(|signal| signal.signal_id == needs_input_id),
            "NeedsInput signal must be preserved over evicted Findings"
        );
    }

    const TEST_RESUME_MAX: u32 = 6;
    const TEST_RESUME_WINDOW_MS: u64 = 600_000;

    #[test]
    fn resume_gate_arms_only_when_idle_eligible_and_under_budget() {
        // Pins: the resume-eligibility gate arms a pending resume only for an idle
        // coordinator on a resume-eligible IfIdle signal under budget, and never
        // dispatches a turn (it only mutates VO state).
        use moa_core::{ChildSignalKind, ParentResumePolicy};
        let now = Utc::now();

        let mut idle = SessionVoState::default();
        let signal = resume_signal(ChildSignalKind::Blocked, ParentResumePolicy::IfIdle);
        assert!(idle.maybe_arm_parent_resume(
            &signal,
            None,
            now,
            TEST_RESUME_MAX,
            TEST_RESUME_WINDOW_MS
        ));
        assert_eq!(idle.pending_parent_resume_signal, Some(signal.signal_id));

        let mut busy = SessionVoState::default();
        assert!(!busy.maybe_arm_parent_resume(
            &signal,
            Some("turn-1"),
            now,
            TEST_RESUME_MAX,
            TEST_RESUME_WINDOW_MS
        ));
        assert_eq!(busy.pending_parent_resume_signal, None);

        let mut finding = SessionVoState::default();
        let finding_signal = resume_signal(ChildSignalKind::Finding, ParentResumePolicy::IfIdle);
        assert!(!finding.maybe_arm_parent_resume(
            &finding_signal,
            None,
            now,
            TEST_RESUME_MAX,
            TEST_RESUME_WINDOW_MS
        ));
        assert_eq!(finding.pending_parent_resume_signal, None);

        let mut never = SessionVoState::default();
        let never_signal = resume_signal(ChildSignalKind::Blocked, ParentResumePolicy::Never);
        assert!(!never.maybe_arm_parent_resume(
            &never_signal,
            None,
            now,
            TEST_RESUME_MAX,
            TEST_RESUME_WINDOW_MS
        ));
        assert_eq!(never.pending_parent_resume_signal, None);

        let mut exhausted = SessionVoState::default();
        exhausted.resume_budget.window_start = Some(now);
        exhausted.resume_budget.count = TEST_RESUME_MAX;
        assert!(!exhausted.maybe_arm_parent_resume(
            &signal,
            None,
            now,
            TEST_RESUME_MAX,
            TEST_RESUME_WINDOW_MS
        ));
        assert_eq!(exhausted.pending_parent_resume_signal, None);
    }

    #[test]
    fn resume_gate_does_not_rearm_once_a_resume_turn_is_active() {
        // Pins: after a resume is dispatched (turn active), a repeated delivery of the
        // same signal does not arm a second resume — the active-turn gate blocks it.
        use moa_core::{ChildSignalKind, ParentResumePolicy};
        let now = Utc::now();
        let signal = resume_signal(ChildSignalKind::Blocked, ParentResumePolicy::IfIdle);

        let mut state = SessionVoState::default();
        assert!(state.maybe_arm_parent_resume(
            &signal,
            None,
            now,
            TEST_RESUME_MAX,
            TEST_RESUME_WINDOW_MS
        ));
        state.record_resume_dispatch("resume-turn".to_string(), now, TEST_RESUME_WINDOW_MS);

        // The dispatched resume turn is now active; a retried signal cannot re-arm.
        assert!(!state.maybe_arm_parent_resume(
            &signal,
            Some("resume-turn"),
            now,
            TEST_RESUME_MAX,
            TEST_RESUME_WINDOW_MS
        ));
        assert_eq!(state.pending_parent_resume_signal, Some(signal.signal_id));
        assert_eq!(state.resume_budget.count, 1);
    }

    #[test]
    fn resume_budget_window_resets_after_elapsed_window() {
        // Pins: the rolling resume budget caps within a window but reopens once the
        // window elapses, and a zero cap disables resume entirely.
        let base = Utc::now();
        let mut budget = super::ResumeBudget::default();
        for _ in 0..TEST_RESUME_MAX {
            assert!(budget.allows(base, TEST_RESUME_WINDOW_MS, TEST_RESUME_MAX));
            budget.consume(base, TEST_RESUME_WINDOW_MS);
        }
        // Cap reached inside the window.
        assert!(!budget.allows(base, TEST_RESUME_WINDOW_MS, TEST_RESUME_MAX));
        // After the window elapses the cap reopens.
        let later = base + chrono::Duration::milliseconds(TEST_RESUME_WINDOW_MS as i64 + 1);
        assert!(budget.allows(later, TEST_RESUME_WINDOW_MS, TEST_RESUME_MAX));
        // A zero cap disables resume regardless of window state.
        assert!(!budget.allows(base, TEST_RESUME_WINDOW_MS, 0));
    }

    #[test]
    fn child_liveness_is_single_outstanding_with_monotonic_generations() {
        // Pins: arming a child's liveness check is single-outstanding (a second arm while
        // one is outstanding is a no-op), generations are monotonic so a re-armed child
        // never reuses a prior generation, and a fired check only matches the live
        // generation of an outstanding entry.
        let mut state = SessionVoState::default();

        let first = state
            .arm_child_liveness("child-1")
            .expect("first arm schedules a check");
        // Single-outstanding: a second arm while one is outstanding does not reschedule.
        assert_eq!(state.arm_child_liveness("child-1"), None);
        // The live generation matches; a superseded/older generation does not.
        assert!(state.liveness_generation_matches("child-1", first));
        assert!(!state.liveness_generation_matches("child-1", first.wrapping_sub(1)));
        assert!(!state.liveness_generation_matches("missing", first));

        // A distinct active child gets its own, strictly newer generation.
        let other = state
            .arm_child_liveness("child-2")
            .expect("second child arms independently");
        assert_ne!(first, other);

        // Clearing (terminal/stale/removed) stops scheduling; a stray tick no longer matches.
        state.clear_child_liveness("child-1");
        assert!(!state.liveness_generation_matches("child-1", first));

        // Re-arming after a clear draws a fresh, strictly newer generation, so any stray
        // in-flight tick carrying `first` can never match the re-armed child.
        let rearmed = state
            .arm_child_liveness("child-1")
            .expect("re-arm after clear schedules a new check");
        assert_ne!(first, rearmed);
        assert!(rearmed > other);
        assert!(!state.liveness_generation_matches("child-1", first));
        assert!(state.liveness_generation_matches("child-1", rearmed));
    }

    #[test]
    fn remove_child_clears_outstanding_liveness_check() {
        // Pins: removing a child (e.g. on self-clean) drops its outstanding liveness
        // watchdog so a later fired check recognizes it as superseded.
        let mut state = SessionVoState::default();
        state.register_child(moa_core::WorkerChildRef {
            id: "child-1".to_string(),
            task_hash: "hash-1".to_string(),
            budget_tokens: 128,
            terminal: None,
        });
        let generation = state
            .arm_child_liveness("child-1")
            .expect("active child arms a liveness check");
        assert!(state.liveness_generation_matches("child-1", generation));

        assert!(state.remove_child("child-1"));
        assert!(!state.liveness_generation_matches("child-1", generation));
    }

    #[test]
    fn clear_resume_on_outcome_drains_only_dispatch_snapshot() {
        // Pins: completing the resume turn drains exactly the dispatch-time unread
        // snapshot and clears the pending signal, leaving mid-turn arrivals queued.
        use moa_core::ChildSignalKind;
        let now = Utc::now();
        let mut state = SessionVoState::default();
        let snap_a = moa_core::AgentSignalId::new();
        let snap_b = moa_core::AgentSignalId::new();
        state.push_unread_child_signal(unread_entry(snap_a, ChildSignalKind::Blocked));
        state.push_unread_child_signal(unread_entry(snap_b, ChildSignalKind::NeedsInput));
        state.pending_parent_resume_signal = Some(snap_a);

        state.record_resume_dispatch("resume-turn".to_string(), now, TEST_RESUME_WINDOW_MS);
        assert_eq!(state.resume_budget.count, 1);

        // A signal that arrives mid-turn must NOT be drained on outcome.
        let mid_turn = moa_core::AgentSignalId::new();
        state.push_unread_child_signal(unread_entry(mid_turn, ChildSignalKind::Finding));

        // A non-matching turn id is a no-op.
        assert!(!state.clear_resume_on_outcome("other-turn"));
        assert!(state.resume_turn.is_some());

        assert!(state.clear_resume_on_outcome("resume-turn"));
        assert_eq!(state.pending_parent_resume_signal, None);
        assert!(state.resume_turn.is_none());
        let remaining: Vec<_> = state
            .unread_child_signals
            .iter()
            .map(|signal| signal.signal_id)
            .collect();
        assert_eq!(remaining, vec![mid_turn]);
    }
}

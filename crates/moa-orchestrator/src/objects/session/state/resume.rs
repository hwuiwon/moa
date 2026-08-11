//! Session resume state behavior.

use super::*;

impl SessionVoState {
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

    /// Clears one unread child signal by id.
    pub fn clear_unread_child_signal(&mut self, signal_id: AgentSignalId) -> bool {
        let before = self.unread_child_signals.len();
        self.unread_child_signals
            .retain(|signal| signal.signal_id != signal_id);
        if self.pending_parent_resume_signal == Some(signal_id) {
            self.pending_parent_resume_signal = None;
        }
        self.unread_child_signals.len() != before
    }

    /// Drains all queued child signals when a coordinator turn is admitted.
    ///
    /// The durable event log still carries those signals into the turn's compiled history;
    /// this only clears the compact VO projection so answered/seen signals do not fill the
    /// bounded unread window after an active turn has had a chance to observe them.
    pub fn drain_unread_child_signals(&mut self) -> usize {
        let drained = self.unread_child_signals.len();
        self.unread_child_signals.clear();
        self.pending_parent_resume_signal = None;
        drained
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

    /// Returns whether the only reason this signal cannot arm a resume is budget.
    #[must_use]
    pub fn resume_budget_exhausted_for_signal(
        &self,
        signal: &WorkerSignal,
        active_turn_id: Option<&str>,
        now: DateTime<Utc>,
        max_per_window: u32,
        window_ms: u64,
    ) -> bool {
        matches!(signal.resume_policy, ParentResumePolicy::IfIdle)
            && signal_kind_is_resume_eligible(signal.kind)
            && active_turn_id.is_none()
            && !self.resume_budget.allows(now, window_ms, max_per_window)
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
}

/// Whether a signal kind must be preserved over informational kinds during unread-cap
/// eviction. Action-required kinds block the child until the coordinator responds.
#[must_use]
fn signal_kind_is_action_required(kind: ChildSignalKind) -> bool {
    matches!(kind, ChildSignalKind::NeedsInput | ChildSignalKind::Blocked)
}

/// Whether a signal kind is eligible to wake an idle coordinator.
///
/// Only blocking, attention, and failure kinds qualify; findings stay passive.
#[must_use]
pub(in crate::objects::session) fn signal_kind_is_resume_eligible(kind: ChildSignalKind) -> bool {
    matches!(
        kind,
        ChildSignalKind::Blocked
            | ChildSignalKind::NeedsInput
            | ChildSignalKind::Failed
            | ChildSignalKind::HeartbeatStale
            | ChildSignalKind::FanInSettled
    )
}

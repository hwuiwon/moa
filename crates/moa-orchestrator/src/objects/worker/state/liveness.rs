//! Worker-owned heartbeat and liveness-deadline state transitions.

use super::*;

/// Deterministic action for one fired Worker-owned liveness deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::objects::worker) enum WorkerLivenessDecision {
    /// The worker is terminal, awaiting input, disabled, or has no heartbeat baseline.
    Stop,
    /// The heartbeat moved the effective deadline; schedule one exact successor.
    Reschedule {
        /// Latest heartbeat plus the configured stale threshold.
        deadline_at: DateTime<Utc>,
    },
    /// The latest heartbeat reached its deadline while the worker remained active.
    Stale {
        /// Exact heartbeat used by the event and its idempotency key.
        last_heartbeat_at: DateTime<Utc>,
    },
}

impl WorkerVoState {
    /// Advances the heartbeat monotonically, ignoring delayed older writes.
    pub(in crate::objects::worker) fn observe_heartbeat(&mut self, at: DateTime<Utc>) {
        if self.last_heartbeat_at.is_none_or(|current| at > current) {
            self.last_heartbeat_at = Some(at);
        }
    }

    /// Arms the first Worker-owned liveness deadline when none is outstanding.
    ///
    /// Returns the owning generation and exact deadline. A zero threshold disables
    /// liveness, and terminal or input-blocked workers never arm.
    pub(in crate::objects::worker) fn arm_liveness_deadline(
        &mut self,
        stale_threshold_ms: u64,
    ) -> Option<(u64, DateTime<Utc>)> {
        if self.liveness_outstanding
            || stale_threshold_ms == 0
            || self.current_status() != WorkerState::Running
            || !self.pending_input_requests.is_empty()
        {
            return None;
        }
        let last_heartbeat_at = self.last_heartbeat_at?;
        self.liveness_generation = self.liveness_generation.saturating_add(1);
        self.liveness_outstanding = true;
        Some((
            self.liveness_generation,
            heartbeat_deadline(last_heartbeat_at, stale_threshold_ms),
        ))
    }

    /// Returns whether a fired deadline still owns the single outstanding slot.
    #[must_use]
    pub(in crate::objects::worker) fn liveness_generation_matches(
        &self,
        expected_generation: u64,
    ) -> bool {
        self.liveness_outstanding && self.liveness_generation == expected_generation
    }

    /// Replaces a fired fresh deadline with one new generation before scheduling.
    pub(in crate::objects::worker) fn replace_liveness_deadline(&mut self) -> u64 {
        self.liveness_generation = self.liveness_generation.saturating_add(1);
        self.liveness_outstanding = true;
        self.liveness_generation
    }

    /// Releases the single outstanding liveness slot.
    pub(in crate::objects::worker) fn stop_liveness_deadline(&mut self) {
        self.liveness_outstanding = false;
    }

    /// Decides a fired deadline from only Worker-owned state and journaled time.
    #[must_use]
    pub(in crate::objects::worker) fn liveness_decision(
        &self,
        now: DateTime<Utc>,
        stale_threshold_ms: u64,
    ) -> WorkerLivenessDecision {
        if stale_threshold_ms == 0
            || self.current_status() != WorkerState::Running
            || !self.pending_input_requests.is_empty()
        {
            return WorkerLivenessDecision::Stop;
        }
        let Some(last_heartbeat_at) = self.last_heartbeat_at else {
            return WorkerLivenessDecision::Stop;
        };
        let deadline_at = heartbeat_deadline(last_heartbeat_at, stale_threshold_ms);
        if now >= deadline_at {
            WorkerLivenessDecision::Stale { last_heartbeat_at }
        } else {
            WorkerLivenessDecision::Reschedule { deadline_at }
        }
    }
}

/// Adds the configured threshold without allowing an oversized configuration to panic.
fn heartbeat_deadline(last_heartbeat_at: DateTime<Utc>, stale_threshold_ms: u64) -> DateTime<Utc> {
    let millis = i64::try_from(stale_threshold_ms).unwrap_or(i64::MAX);
    last_heartbeat_at
        .checked_add_signed(chrono::Duration::milliseconds(millis))
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

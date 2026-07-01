//! Per-turn coordination instrumentation: durable virtual-object round-trips.
//!
//! A "coordination round-trip" is a blocking `.call()` (or fire-and-forget `.send()`) from the
//! turn workflow to a Session/Worker virtual object — the durable-execution cost that fan-in and
//! delegation optimizations (single-owner fan-in, wait fast-path, never-terminal recovery) target.
//! These hops are otherwise invisible to MOA telemetry, so this module counts them per turn in a
//! task-local scope, mirroring [`crate::session_replay::TurnReplayCounters`]. Outside an active
//! [`scope_coordination_counters`] scope every recorder is a no-op.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

tokio::task_local! {
    static COORDINATION_COUNTERS: Arc<CoordinationCounters>;
}

/// Read-only snapshot of per-turn coordination counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoordinationSnapshot {
    /// Blocking `.call()` round-trips to the Session virtual object.
    pub session_vo_calls: u64,
    /// Blocking `.call()` round-trips to a Worker virtual object.
    pub worker_vo_calls: u64,
    /// Fire-and-forget `.send()` dispatches to any virtual object (no reply awaited).
    pub vo_sends: u64,
    /// Durable event appends made during the turn (a proxy for journal/replay footprint).
    pub durable_appends: u64,
}

impl CoordinationSnapshot {
    /// Total blocking VO round-trips (Session + Worker) — the primary latency/replay cost.
    #[must_use]
    pub fn total_vo_calls(&self) -> u64 {
        self.session_vo_calls.saturating_add(self.worker_vo_calls)
    }
}

/// Mutable per-turn coordination counters held in task-local scope.
#[derive(Debug, Default)]
pub struct CoordinationCounters {
    session_vo_calls: AtomicU64,
    worker_vo_calls: AtomicU64,
    vo_sends: AtomicU64,
    durable_appends: AtomicU64,
}

impl CoordinationCounters {
    /// Returns a read-only snapshot of the current counter values.
    #[must_use]
    pub fn snapshot(&self) -> CoordinationSnapshot {
        CoordinationSnapshot {
            session_vo_calls: self.session_vo_calls.load(Ordering::Relaxed),
            worker_vo_calls: self.worker_vo_calls.load(Ordering::Relaxed),
            vo_sends: self.vo_sends.load(Ordering::Relaxed),
            durable_appends: self.durable_appends.load(Ordering::Relaxed),
        }
    }
}

/// Runs a future inside a fresh per-turn coordination-counter scope.
pub async fn scope_coordination_counters<F, T>(counters: Arc<CoordinationCounters>, future: F) -> T
where
    F: Future<Output = T>,
{
    COORDINATION_COUNTERS.scope(counters, future).await
}

/// Records one blocking Session-VO round-trip for the current turn (no-op outside scope).
pub fn record_session_vo_call() {
    let _ = COORDINATION_COUNTERS.try_with(|counters| {
        counters.session_vo_calls.fetch_add(1, Ordering::Relaxed);
    });
}

/// Records one blocking Worker-VO round-trip for the current turn (no-op outside scope).
pub fn record_worker_vo_call() {
    let _ = COORDINATION_COUNTERS.try_with(|counters| {
        counters.worker_vo_calls.fetch_add(1, Ordering::Relaxed);
    });
}

/// Records one fire-and-forget VO dispatch for the current turn (no-op outside scope).
pub fn record_vo_send() {
    let _ = COORDINATION_COUNTERS.try_with(|counters| {
        counters.vo_sends.fetch_add(1, Ordering::Relaxed);
    });
}

/// Records one durable event append for the current turn (no-op outside scope).
pub fn record_durable_append() {
    let _ = COORDINATION_COUNTERS.try_with(|counters| {
        counters.durable_appends.fetch_add(1, Ordering::Relaxed);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn coordination_counters_record_within_scope() {
        let counters = Arc::new(CoordinationCounters::default());
        scope_coordination_counters(counters.clone(), async {
            record_session_vo_call();
            record_session_vo_call();
            record_worker_vo_call();
            record_vo_send();
            record_durable_append();
        })
        .await;

        let snapshot = counters.snapshot();
        assert_eq!(snapshot.session_vo_calls, 2);
        assert_eq!(snapshot.worker_vo_calls, 1);
        assert_eq!(snapshot.vo_sends, 1);
        assert_eq!(snapshot.durable_appends, 1);
        assert_eq!(snapshot.total_vo_calls(), 3);
    }

    #[tokio::test]
    async fn coordination_recording_is_noop_outside_scope() {
        record_session_vo_call();
        record_worker_vo_call();
        let counters = Arc::new(CoordinationCounters::default());
        assert_eq!(counters.snapshot(), CoordinationSnapshot::default());
    }
}

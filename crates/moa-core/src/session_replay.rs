//! Per-turn session event replay instrumentation utilities.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

tokio::task_local! {
    static TURN_REPLAY_COUNTERS: Arc<TurnReplayCounters>;
}

/// Snapshot of per-turn event replay counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnReplaySnapshot {
    /// Number of `get_events` calls made during the turn.
    pub get_events_calls: u64,
    /// Total number of event records returned across all `get_events` calls.
    pub events_replayed: u64,
    /// Approximate number of bytes deserialized across all returned events.
    pub events_bytes: u64,
    /// Aggregate wall-clock time spent inside `get_events`.
    pub get_events_total_duration: Duration,
    /// Aggregate wall-clock time spent compiling pipeline context for the turn.
    pub pipeline_compile_duration: Duration,
}

impl TurnReplaySnapshot {
    /// Returns total `get_events` time in whole milliseconds.
    pub fn get_events_total_ms(&self) -> u64 {
        display_duration_ms(self.get_events_total_duration)
    }

    /// Returns total pipeline compile time in whole milliseconds.
    pub fn pipeline_compile_ms(&self) -> u64 {
        display_duration_ms(self.pipeline_compile_duration)
    }
}

/// Mutable per-turn counters stored in task-local scope.
#[derive(Debug, Default)]
pub struct TurnReplayCounters {
    get_events_calls: AtomicU64,
    events_replayed: AtomicU64,
    events_bytes: AtomicU64,
    get_events_total_us: AtomicU64,
    pipeline_compile_us: AtomicU64,
}

impl TurnReplayCounters {
    /// Returns a read-only snapshot of the current counter values.
    pub fn snapshot(&self) -> TurnReplaySnapshot {
        TurnReplaySnapshot {
            get_events_calls: self.get_events_calls.load(Ordering::Relaxed),
            events_replayed: self.events_replayed.load(Ordering::Relaxed),
            events_bytes: self.events_bytes.load(Ordering::Relaxed),
            get_events_total_duration: Duration::from_micros(
                self.get_events_total_us.load(Ordering::Relaxed),
            ),
            pipeline_compile_duration: Duration::from_micros(
                self.pipeline_compile_us.load(Ordering::Relaxed),
            ),
        }
    }

    fn record_get_events(&self, event_count: usize, bytes: u64, duration: Duration) {
        self.get_events_calls.fetch_add(1, Ordering::Relaxed);
        self.events_replayed
            .fetch_add(event_count as u64, Ordering::Relaxed);
        self.events_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.get_events_total_us
            .fetch_add(recorded_duration_micros(duration), Ordering::Relaxed);
    }

    fn record_pipeline_compile_duration(&self, duration: Duration) {
        self.pipeline_compile_us
            .fetch_add(recorded_duration_micros(duration), Ordering::Relaxed);
    }
}

/// Runs a future inside a fresh per-turn replay-counter scope.
pub async fn scope_turn_replay_counters<F, T>(counters: Arc<TurnReplayCounters>, future: F) -> T
where
    F: Future<Output = T>,
{
    TURN_REPLAY_COUNTERS.scope(counters, future).await
}

/// Records pipeline compilation time for the current turn when instrumentation is active.
pub fn record_pipeline_compile_duration(duration: Duration) {
    let _ = TURN_REPLAY_COUNTERS.try_with(|counters| {
        counters.record_pipeline_compile_duration(duration);
    });
}

/// Records one `get_events` load in the current turn's replay counters.
///
/// Session stores call this after each event load; outside an active
/// `scope_turn_replay_counters` scope it is a no-op.
pub fn record_session_event_replay(event_count: usize, bytes: u64, duration: Duration) {
    let _ = TURN_REPLAY_COUNTERS.try_with(|counters| {
        counters.record_get_events(event_count, bytes, duration);
    });
}

fn recorded_duration_micros(duration: Duration) -> u64 {
    duration.as_micros().max(1) as u64
}

fn display_duration_ms(duration: Duration) -> u64 {
    let millis = duration.as_millis() as u64;
    if millis == 0 && duration > Duration::ZERO {
        1
    } else {
        millis
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replay_counters_record_within_scope() {
        let counters = Arc::new(TurnReplayCounters::default());

        scope_turn_replay_counters(counters.clone(), async {
            record_session_event_replay(1, 64, Duration::from_micros(250));
            record_pipeline_compile_duration(Duration::from_millis(12));
        })
        .await;

        let snapshot = counters.snapshot();
        assert_eq!(snapshot.get_events_calls, 1);
        assert_eq!(snapshot.events_replayed, 1);
        assert_eq!(snapshot.events_bytes, 64);
        assert!(snapshot.get_events_total_duration > Duration::ZERO);
        assert_eq!(snapshot.pipeline_compile_ms(), 12);
    }

    #[tokio::test]
    async fn replay_recording_is_noop_outside_scope() {
        record_session_event_replay(3, 128, Duration::from_micros(50));
        record_pipeline_compile_duration(Duration::from_millis(5));

        let counters = Arc::new(TurnReplayCounters::default());
        let snapshot = counters.snapshot();
        assert_eq!(snapshot.get_events_calls, 0);
        assert_eq!(snapshot.events_replayed, 0);
    }
}

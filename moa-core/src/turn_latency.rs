//! Per-turn latency instrumentation utilities shared across orchestration layers.

use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

tokio::task_local! {
    static TURN_LATENCY_COUNTERS: Arc<TurnLatencyCounters>;
}

/// Snapshot of per-turn latency breakdown metrics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnLatencySnapshot {
    /// Time spent loading and deserializing the turn-start snapshot.
    pub snapshot_load_duration: Duration,
    /// Whether the current turn successfully reused a stored snapshot.
    pub snapshot_hit: bool,
    /// Time spent serializing and persisting the refreshed snapshot.
    pub snapshot_write_duration: Duration,
    /// Aggregate time spent compiling context for the turn.
    pub pipeline_compile_duration: Duration,
    /// Aggregate time spent in provider completion/stream handling.
    pub llm_call_duration: Duration,
    /// Aggregate time spent dispatching tools for the turn.
    pub tool_dispatch_duration: Duration,
    /// Aggregate time spent persisting turn events and status writes.
    pub event_persist_duration: Duration,
    /// Time-to-first-token for the first streamed provider response in the turn.
    pub llm_ttft: Option<Duration>,
    /// Number of tool calls executed or denied within tool-dispatch segments.
    pub tool_calls: u64,
    /// Number of persisted event writes recorded during the turn.
    pub events_written: u64,
    /// Whether deterministic compaction ran for the turn.
    pub compaction_tier1: bool,
    /// Whether cache-aware trimming ran for the turn.
    pub compaction_tier2: bool,
    /// Whether summarization compaction ran for the turn.
    pub compaction_tier3: bool,
    /// Approximate input tokens reclaimed by compaction.
    pub compaction_tokens_reclaimed: u64,
    /// Number of message payloads replaced or elided by compaction.
    pub compaction_messages_elided: u64,
}

impl TurnLatencySnapshot {
    /// Returns snapshot load time in whole milliseconds.
    pub fn snapshot_load_ms(&self) -> u64 {
        display_duration_ms(self.snapshot_load_duration)
    }

    /// Returns snapshot write time in whole milliseconds.
    pub fn snapshot_write_ms(&self) -> u64 {
        display_duration_ms(self.snapshot_write_duration)
    }

    /// Returns pipeline compile time in whole milliseconds.
    pub fn pipeline_compile_ms(&self) -> u64 {
        display_duration_ms(self.pipeline_compile_duration)
    }

    /// Returns LLM call time in whole milliseconds.
    pub fn llm_call_ms(&self) -> u64 {
        display_duration_ms(self.llm_call_duration)
    }

    /// Returns tool dispatch time in whole milliseconds.
    pub fn tool_dispatch_ms(&self) -> u64 {
        display_duration_ms(self.tool_dispatch_duration)
    }

    /// Returns event persistence time in whole milliseconds.
    pub fn event_persist_ms(&self) -> u64 {
        display_duration_ms(self.event_persist_duration)
    }

    /// Returns TTFT in whole milliseconds when observed.
    pub fn llm_ttft_ms(&self) -> Option<u64> {
        self.llm_ttft.map(display_duration_ms)
    }
}

/// Mutable per-turn latency counters stored in task-local scope.
#[derive(Debug)]
pub struct TurnLatencyCounters {
    root_turn_span: tracing::Span,
    snapshot_load_us: AtomicU64,
    snapshot_hit: AtomicU64,
    snapshot_write_us: AtomicU64,
    pipeline_compile_us: AtomicU64,
    llm_call_us: AtomicU64,
    tool_dispatch_us: AtomicU64,
    event_persist_us: AtomicU64,
    tool_calls: AtomicU64,
    events_written: AtomicU64,
    compaction_tier1: AtomicU64,
    compaction_tier2: AtomicU64,
    compaction_tier3: AtomicU64,
    compaction_tokens_reclaimed: AtomicU64,
    compaction_messages_elided: AtomicU64,
    llm_ttft_us: Mutex<Option<u64>>,
}

impl TurnLatencyCounters {
    /// Creates a new per-turn latency tracker rooted at the provided turn span.
    pub fn new(root_turn_span: tracing::Span) -> Self {
        Self {
            root_turn_span,
            snapshot_load_us: AtomicU64::new(0),
            snapshot_hit: AtomicU64::new(0),
            snapshot_write_us: AtomicU64::new(0),
            pipeline_compile_us: AtomicU64::new(0),
            llm_call_us: AtomicU64::new(0),
            tool_dispatch_us: AtomicU64::new(0),
            event_persist_us: AtomicU64::new(0),
            tool_calls: AtomicU64::new(0),
            events_written: AtomicU64::new(0),
            compaction_tier1: AtomicU64::new(0),
            compaction_tier2: AtomicU64::new(0),
            compaction_tier3: AtomicU64::new(0),
            compaction_tokens_reclaimed: AtomicU64::new(0),
            compaction_messages_elided: AtomicU64::new(0),
            llm_ttft_us: Mutex::new(None),
        }
    }

    /// Returns a read-only snapshot of the current latency metrics.
    pub fn snapshot(&self) -> TurnLatencySnapshot {
        let llm_ttft = self
            .llm_ttft_us
            .lock()
            .ok()
            .and_then(|guard| *guard)
            .map(Duration::from_micros);
        TurnLatencySnapshot {
            snapshot_load_duration: Duration::from_micros(
                self.snapshot_load_us.load(Ordering::Relaxed),
            ),
            snapshot_hit: self.snapshot_hit.load(Ordering::Relaxed) > 0,
            snapshot_write_duration: Duration::from_micros(
                self.snapshot_write_us.load(Ordering::Relaxed),
            ),
            pipeline_compile_duration: Duration::from_micros(
                self.pipeline_compile_us.load(Ordering::Relaxed),
            ),
            llm_call_duration: Duration::from_micros(self.llm_call_us.load(Ordering::Relaxed)),
            tool_dispatch_duration: Duration::from_micros(
                self.tool_dispatch_us.load(Ordering::Relaxed),
            ),
            event_persist_duration: Duration::from_micros(
                self.event_persist_us.load(Ordering::Relaxed),
            ),
            llm_ttft,
            tool_calls: self.tool_calls.load(Ordering::Relaxed),
            events_written: self.events_written.load(Ordering::Relaxed),
            compaction_tier1: self.compaction_tier1.load(Ordering::Relaxed) > 0,
            compaction_tier2: self.compaction_tier2.load(Ordering::Relaxed) > 0,
            compaction_tier3: self.compaction_tier3.load(Ordering::Relaxed) > 0,
            compaction_tokens_reclaimed: self.compaction_tokens_reclaimed.load(Ordering::Relaxed),
            compaction_messages_elided: self.compaction_messages_elided.load(Ordering::Relaxed),
        }
    }

    fn root_turn_span(&self) -> tracing::Span {
        self.root_turn_span.clone()
    }

    fn record_pipeline_compile_duration(&self, duration: Duration) {
        self.pipeline_compile_us
            .fetch_add(duration.as_micros() as u64, Ordering::Relaxed);
    }

    fn record_snapshot_load(&self, duration: Duration, hit: bool) {
        self.snapshot_load_us
            .fetch_add(duration.as_micros() as u64, Ordering::Relaxed);
        if hit {
            self.snapshot_hit.store(1, Ordering::Relaxed);
        }
    }

    fn record_snapshot_write_duration(&self, duration: Duration) {
        self.snapshot_write_us
            .fetch_add(duration.as_micros() as u64, Ordering::Relaxed);
    }

    fn record_llm_call_duration(&self, duration: Duration) {
        self.llm_call_us
            .fetch_add(duration.as_micros() as u64, Ordering::Relaxed);
    }

    fn record_tool_dispatch_duration(&self, duration: Duration, tool_calls: usize) {
        self.tool_dispatch_us
            .fetch_add(duration.as_micros() as u64, Ordering::Relaxed);
        self.tool_calls
            .fetch_add(tool_calls as u64, Ordering::Relaxed);
    }

    fn record_event_persist_duration(&self, duration: Duration, events_written: usize) {
        self.event_persist_us
            .fetch_add(duration.as_micros() as u64, Ordering::Relaxed);
        self.events_written
            .fetch_add(events_written as u64, Ordering::Relaxed);
    }

    fn record_llm_ttft(&self, duration: Duration) {
        if let Ok(mut guard) = self.llm_ttft_us.lock()
            && guard.is_none()
        {
            *guard = Some(duration.as_micros() as u64);
        }
    }

    fn record_compaction(
        &self,
        tier1: bool,
        tier2: bool,
        tier3: bool,
        tokens_reclaimed: usize,
        messages_elided: usize,
    ) {
        if tier1 {
            self.compaction_tier1.store(1, Ordering::Relaxed);
        }
        if tier2 {
            self.compaction_tier2.store(1, Ordering::Relaxed);
        }
        if tier3 {
            self.compaction_tier3.store(1, Ordering::Relaxed);
        }
        self.compaction_tokens_reclaimed
            .fetch_add(tokens_reclaimed as u64, Ordering::Relaxed);
        self.compaction_messages_elided
            .fetch_add(messages_elided as u64, Ordering::Relaxed);
    }
}

/// Runs a future inside a per-turn latency scope rooted at the supplied turn span.
pub async fn scope_turn_latency_counters<F, T>(counters: Arc<TurnLatencyCounters>, future: F) -> T
where
    F: Future<Output = T>,
{
    TURN_LATENCY_COUNTERS.scope(counters, future).await
}

/// Returns the current turn root span when latency instrumentation is active.
pub fn current_turn_root_span() -> Option<tracing::Span> {
    TURN_LATENCY_COUNTERS
        .try_with(|counters| counters.root_turn_span())
        .ok()
}

/// Records pipeline compile duration for the current turn.
pub fn record_turn_pipeline_compile_duration(duration: Duration) {
    let _ = TURN_LATENCY_COUNTERS.try_with(|counters| {
        counters.record_pipeline_compile_duration(duration);
    });
}

/// Records snapshot load duration and whether the snapshot was a usable hit.
pub fn record_turn_snapshot_load(duration: Duration, hit: bool) {
    let _ = TURN_LATENCY_COUNTERS.try_with(|counters| {
        counters.record_snapshot_load(duration, hit);
    });
}

/// Records snapshot serialization and persistence time for the current turn.
pub fn record_turn_snapshot_write_duration(duration: Duration) {
    let _ = TURN_LATENCY_COUNTERS.try_with(|counters| {
        counters.record_snapshot_write_duration(duration);
    });
}

/// Records LLM call duration for the current turn.
pub fn record_turn_llm_call_duration(duration: Duration) {
    let _ = TURN_LATENCY_COUNTERS.try_with(|counters| {
        counters.record_llm_call_duration(duration);
    });
}

/// Records tool dispatch duration and tool count for the current turn.
pub fn record_turn_tool_dispatch_duration(duration: Duration, tool_calls: usize) {
    let _ = TURN_LATENCY_COUNTERS.try_with(|counters| {
        counters.record_tool_dispatch_duration(duration, tool_calls);
    });
}

/// Records event persistence duration and write count for the current turn.
pub fn record_turn_event_persist_duration(duration: Duration, events_written: usize) {
    let _ = TURN_LATENCY_COUNTERS.try_with(|counters| {
        counters.record_event_persist_duration(duration, events_written);
    });
}

/// Records first-token latency for the current turn when it is first observed.
pub fn record_turn_llm_ttft(duration: Duration) {
    let _ = TURN_LATENCY_COUNTERS.try_with(|counters| {
        counters.record_llm_ttft(duration);
    });
}

/// Records compaction activity for the current turn.
pub fn record_turn_compaction(
    tier1: bool,
    tier2: bool,
    tier3: bool,
    tokens_reclaimed: usize,
    messages_elided: usize,
) {
    let _ = TURN_LATENCY_COUNTERS.try_with(|counters| {
        counters.record_compaction(tier1, tier2, tier3, tokens_reclaimed, messages_elided);
    });
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
    async fn turn_latency_scope_records_all_metrics() {
        let counters = Arc::new(TurnLatencyCounters::new(tracing::Span::none()));

        scope_turn_latency_counters(counters.clone(), async {
            record_turn_snapshot_load(Duration::from_millis(2), true);
            record_turn_snapshot_write_duration(Duration::from_millis(4));
            record_turn_pipeline_compile_duration(Duration::from_millis(12));
            record_turn_llm_call_duration(Duration::from_millis(33));
            record_turn_tool_dispatch_duration(Duration::from_millis(7), 2);
            record_turn_event_persist_duration(Duration::from_millis(5), 3);
            record_turn_llm_ttft(Duration::from_millis(9));
            record_turn_llm_ttft(Duration::from_millis(20));
            record_turn_compaction(true, false, true, 42, 3);
        })
        .await;

        let snapshot = counters.snapshot();
        assert_eq!(snapshot.snapshot_load_ms(), 2);
        assert!(snapshot.snapshot_hit);
        assert_eq!(snapshot.snapshot_write_ms(), 4);
        assert_eq!(snapshot.pipeline_compile_ms(), 12);
        assert_eq!(snapshot.llm_call_ms(), 33);
        assert_eq!(snapshot.tool_dispatch_ms(), 7);
        assert_eq!(snapshot.event_persist_ms(), 5);
        assert_eq!(snapshot.llm_ttft_ms(), Some(9));
        assert_eq!(snapshot.tool_calls, 2);
        assert_eq!(snapshot.events_written, 3);
        assert!(snapshot.compaction_tier1);
        assert!(!snapshot.compaction_tier2);
        assert!(snapshot.compaction_tier3);
        assert_eq!(snapshot.compaction_tokens_reclaimed, 42);
        assert_eq!(snapshot.compaction_messages_elided, 3);
    }
}

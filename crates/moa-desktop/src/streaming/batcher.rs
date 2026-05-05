//! Small time-window batcher for rapid [`RuntimeEvent`] streams.
//!
//! Per-token re-renders tank GPUI performance, so we coalesce events produced
//! within a short window and flush them together.

use std::time::{Duration, Instant};

use moa_core::RuntimeEvent;

/// Accumulates runtime events and yields them in batches on a fixed interval.
pub struct StreamBatcher {
    pending: Vec<RuntimeEvent>,
    last_flush: Instant,
    interval: Duration,
}

impl StreamBatcher {
    /// Creates a batcher with the given flush interval.
    pub fn new(interval: Duration) -> Self {
        Self {
            pending: Vec::new(),
            last_flush: Instant::now(),
            interval,
        }
    }

    /// Adds an event. Returns a batch when the flush window has elapsed.
    pub fn push(&mut self, event: RuntimeEvent) -> Option<Vec<RuntimeEvent>> {
        self.pending.push(event);
        if self.last_flush.elapsed() >= self.interval {
            self.last_flush = Instant::now();
            return Some(std::mem::take(&mut self.pending));
        }
        None
    }

    /// Forces a flush regardless of elapsed time. Returns empty vec if nothing pending.
    pub fn flush(&mut self) -> Vec<RuntimeEvent> {
        self.last_flush = Instant::now();
        std::mem::take(&mut self.pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn batcher_holds_events_until_interval_elapses() {
        let mut batcher = StreamBatcher::new(Duration::from_millis(30));
        assert!(batcher.push(RuntimeEvent::AssistantStarted).is_none());
        assert!(batcher.push(RuntimeEvent::AssistantDelta('a')).is_none());
        thread::sleep(Duration::from_millis(40));
        let batch = batcher.push(RuntimeEvent::AssistantDelta('b'));
        let batch = batch.expect("should flush after interval");
        assert_eq!(batch.len(), 3);
    }

    #[test]
    fn flush_returns_remaining_events() {
        let mut batcher = StreamBatcher::new(Duration::from_millis(1000));
        batcher.push(RuntimeEvent::TurnCompleted);
        let drained = batcher.flush();
        assert_eq!(drained.len(), 1);
        assert!(batcher.flush().is_empty());
    }
}

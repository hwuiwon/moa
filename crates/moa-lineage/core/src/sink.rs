//! Hot-path sink trait for lineage capture.

use crate::records::LineageEvent;

/// Hot-path lineage tap. Cheap to call and never blocks.
///
/// Implementations must not await or perform synchronous I/O in `record`.
/// Production implementations should enqueue the event with one bounded
/// `try_send` and drop with a counter increment when saturated.
pub trait LineageSink: Send + Sync + 'static {
    /// Records an event for asynchronous capture.
    fn record(&self, evt: LineageEvent);

    /// Returns the number of events dropped due to buffer pressure.
    fn dropped_count(&self) -> u64;
}

/// Disabled-cost fallback for lineage capture.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl LineageSink for NullSink {
    fn record(&self, _evt: LineageEvent) {}

    fn dropped_count(&self) -> u64 {
        0
    }
}

//! Real-time streaming: accumulate RuntimeEvent deltas and flush in batches.

pub mod batcher;

pub use batcher::StreamBatcher;

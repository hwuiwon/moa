//! Real-time streaming: accumulate RuntimeEvent deltas and flush in batches.

pub mod batcher;
pub mod heal;

pub use batcher::StreamBatcher;
pub use heal::heal;

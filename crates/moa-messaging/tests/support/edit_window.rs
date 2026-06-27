//! Edit-window test fixtures.

use moa_core::MessageId;

/// Returns a deterministic message id.
pub fn message_id(value: &str) -> MessageId {
    MessageId::new(value)
}

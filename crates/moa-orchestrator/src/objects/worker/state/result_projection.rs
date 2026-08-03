//! Terminal-result history projection helpers.

use super::*;

pub(super) fn latest_assistant_text(history: &[WorkerHistoryEntry]) -> Option<String> {
    history.iter().rev().find_map(|entry| match entry {
        WorkerHistoryEntry::Inline(message)
            if matches!(
                message.role,
                moa_core::types::context::MessageRole::Assistant
            ) && !message.content.trim().is_empty() =>
        {
            Some(message.content.clone())
        }
        // A claimed assistant body is surfaced here only as a last-resort fallback for the
        // terminal result output (reached solely when no `last_turn_summary` was recorded),
        // so the stored preview is sufficient and avoids a blob read on the terminal path.
        WorkerHistoryEntry::Claimed(claimed)
            if matches!(
                claimed.role,
                moa_core::types::context::MessageRole::Assistant
            ) && !claimed.preview.trim().is_empty() =>
        {
            Some(claimed.preview.clone())
        }
        _ => None,
    })
}

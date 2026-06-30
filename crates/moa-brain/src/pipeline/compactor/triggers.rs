//! Compaction trigger helpers and history-bound calculations.

use std::collections::HashSet;

use moa_core::{CompactionConfig, ContextMessage, ContextSnapshot, MessageRole, WorkingContext};

use crate::pipeline::history::{HISTORY_END_INDEX_METADATA_KEY, HISTORY_START_INDEX_METADATA_KEY};
use moa_core::sum_message_tokens;

pub(super) fn history_bounds(ctx: &WorkingContext) -> Option<(usize, usize)> {
    let start = ctx
        .metadata()
        .get(HISTORY_START_INDEX_METADATA_KEY)?
        .as_u64()? as usize;
    let end = ctx
        .metadata()
        .get(HISTORY_END_INDEX_METADATA_KEY)?
        .as_u64()? as usize;
    Some((start, end))
}

pub(super) fn protected_snapshot_tool_use_ids(snapshot: &ContextSnapshot) -> HashSet<String> {
    snapshot
        .file_read_dedup_state
        .latest_reads
        .values()
        .map(|state| state.tool_use_id.clone())
        .collect()
}

pub(super) fn recent_turn_boundary_messages(
    messages: &[ContextMessage],
    recent_turns: usize,
) -> usize {
    if recent_turns == 0 || messages.is_empty() {
        return messages.len();
    }

    let mut turns_seen = 0usize;
    for index in (0..messages.len()).rev() {
        if messages[index].role == MessageRole::User {
            turns_seen += 1;
            if turns_seen == recent_turns {
                return index;
            }
        }
    }

    0
}

pub(super) fn should_apply_tier2(
    history_messages: &[ContextMessage],
    recent_boundary: usize,
    config: &CompactionConfig,
) -> bool {
    if recent_boundary == 0 {
        return false;
    }

    let token_pressure = token_count(history_messages);
    recent_boundary > config.tier2_trigger_blocks_past_bp4
        && token_pressure > config.max_input_tokens_per_turn / 2
}

pub(super) fn token_count(messages: &[ContextMessage]) -> usize {
    sum_message_tokens(messages)
}

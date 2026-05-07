//! Snapshot metadata handling for compactor mutations.

use moa_core::{ContextSnapshot, Result, WorkingContext};
use serde_json::Value;

use super::deterministic::CACHE_COMPACTION_PLACEHOLDER;
use super::triggers::token_count;
use crate::pipeline::history::HISTORY_SNAPSHOT_METADATA_KEY;

pub(super) fn load_snapshot(ctx: &WorkingContext) -> Option<ContextSnapshot> {
    let value = ctx.metadata().get(HISTORY_SNAPSHOT_METADATA_KEY)?;
    if value.is_null() {
        return None;
    }

    serde_json::from_value(value.clone()).ok()
}

pub(super) fn store_snapshot(
    ctx: &mut WorkingContext,
    snapshot: Option<ContextSnapshot>,
) -> Result<()> {
    let value = match snapshot {
        Some(snapshot) => serde_json::to_value(snapshot)?,
        None => Value::Null,
    };
    ctx.insert_metadata(HISTORY_SNAPSHOT_METADATA_KEY, value);
    Ok(())
}

pub(super) fn collapse_snapshot_for_tier2(snapshot: &mut ContextSnapshot) {
    snapshot.messages = vec![moa_core::ContextMessage::system(
        CACHE_COMPACTION_PLACEHOLDER,
    )];
    snapshot.file_read_dedup_state.latest_reads.clear();
    snapshot.token_count = token_count(&snapshot.messages);
}

//! Snapshot types for incremental context compilation.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{ContextMessage, SequenceNum, SessionId, ToolCallId};

/// Current serialized context snapshot format version.
pub const CONTEXT_SNAPSHOT_FORMAT_VERSION: u32 = 4;

/// Serializable cache of compiled history state for one session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextSnapshot {
    /// Snapshot payload format version.
    pub format_version: u32,
    /// Session the snapshot belongs to.
    pub session_id: SessionId,
    /// Last event sequence number included in this snapshot.
    pub last_sequence_num: SequenceNum,
    /// Snapshot creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Compiled history messages produced by the history stage.
    pub messages: Vec<ContextMessage>,
    /// Running state needed to preserve file-read deduplication across turns.
    pub file_read_dedup_state: FileReadDedupState,
    /// Approximate token count for the compiled history messages.
    pub token_count: usize,
    /// Fingerprint of the static pre-history stage inputs.
    pub stage_inputs_hash: u64,
}

impl ContextSnapshot {
    /// Returns whether this snapshot matches the current code format version.
    pub fn is_current_version(&self) -> bool {
        self.format_version == CONTEXT_SNAPSHOT_FORMAT_VERSION
    }
}

/// File-read deduplication state preserved in a snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileReadDedupState {
    /// Latest content-bearing full-file read per path, as of the snapshot's
    /// last sequence number, so incremental replay can extend the dedup walk
    /// without reloading pre-snapshot events.
    pub latest_reads: HashMap<String, SnapshotFileReadState>,
}

/// Identity of the latest content-bearing full-file read for one path.
///
/// History replay never rewrites already-snapshotted messages between
/// checkpoints; a later identical re-read is replaced on the *new* side with a
/// pointer, so this state only needs to identify the content-bearing read and
/// its replayed-text hash for identity comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotFileReadState {
    /// Internal tool call identifier for the content-bearing file-read result.
    pub tool_id: ToolCallId,
    /// BLAKE3 hex digest of the replayed tool output text.
    pub content_hash: String,
}

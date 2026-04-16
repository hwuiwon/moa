//! Snapshot types for incremental context compilation.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{CacheBreakpoint, ContextMessage, SequenceNum, SessionId};

/// Current serialized context snapshot format version.
pub const CONTEXT_SNAPSHOT_FORMAT_VERSION: u32 = 2;

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
    /// Cache markers associated with the compiled request when the snapshot was written.
    #[serde(default)]
    pub cache_controls: Vec<CacheBreakpoint>,
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
    /// Latest full-file read result currently present in the compiled history, keyed by path.
    pub latest_reads: HashMap<String, SnapshotFileReadState>,
}

/// Metadata needed to replace an older full-file read with a placeholder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotFileReadState {
    /// Index of the full-file read tool-result message within `ContextSnapshot.messages`.
    pub message_index: usize,
    /// Provider-visible tool use identifier used to keep tool history structurally valid.
    pub tool_use_id: String,
    /// Internal tool call identifier for the file-read result.
    pub tool_id: Uuid,
    /// Whether the original tool result was successful.
    pub success: bool,
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::{CONTEXT_SNAPSHOT_FORMAT_VERSION, ContextSnapshot, FileReadDedupState};
    use crate::{ContextMessage, SessionId};

    #[test]
    fn context_snapshot_round_trips() {
        let snapshot = ContextSnapshot {
            format_version: CONTEXT_SNAPSHOT_FORMAT_VERSION,
            session_id: SessionId::new(),
            last_sequence_num: 42,
            created_at: Utc::now(),
            messages: vec![
                ContextMessage::user("hello"),
                ContextMessage::assistant("world"),
            ],
            file_read_dedup_state: FileReadDedupState::default(),
            token_count: 12,
            cache_controls: Vec::new(),
            stage_inputs_hash: 1234,
        };

        let encoded = serde_json::to_vec(&snapshot).expect("serialize snapshot");
        let decoded: ContextSnapshot =
            serde_json::from_slice(&encoded).expect("deserialize snapshot");

        assert_eq!(decoded, snapshot);
        assert!(decoded.is_current_version());
    }
}

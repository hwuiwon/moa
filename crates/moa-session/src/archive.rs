//! Archive format for terminal-session event history.
//!
//! This module owns the on-disk contract of `session_event_archives.payload`
//! and the pure decisions around it: what a terminal session is, what the
//! serialized history looks like, how its digest is derived, and how a range
//! filter is applied to hydrated history so replaying an archived session
//! returns exactly what replaying a live one would have returned.
//!
//! Everything here is deterministic and free of I/O. The SQL that writes,
//! verifies, hydrates, and purges archives lives in
//! `crate::store::session_archive`.

use chrono::{DateTime, Utc};
use moa_core::error::{MoaError, Result};
use moa_core::types::events_stream::EventRange;
use moa_core::types::identifiers::{SessionId, TenantId};
use moa_core::types::session::SessionStatus;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Version of the serialized archive body.
///
/// Written into every archive row and re-checked on hydration: an archive
/// written by a future format must be refused loudly rather than decoded into
/// a plausible-looking partial history.
pub const SESSION_ARCHIVE_FORMAT_VERSION: i32 = 1;

/// Length in bytes of the archive content digest.
pub const SESSION_ARCHIVE_DIGEST_LEN: usize = 32;

/// Returns whether a session's history is finished and therefore archivable.
///
/// `Created`, `Running`, and `Paused` sessions can still append; archiving one
/// would capture a prefix and then delete rows the session is still writing to.
#[must_use]
pub fn is_terminal_status(status: &SessionStatus) -> bool {
    match status {
        SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed => true,
        SessionStatus::Created | SessionStatus::Running | SessionStatus::Paused => false,
    }
}

/// Returns the database representations of every terminal session status.
///
/// Derived from [`SessionStatus`] rather than written out as literals, so a new
/// terminal status cannot silently become invisible to the retention scan.
#[must_use]
pub fn terminal_status_strings() -> Vec<String> {
    [
        SessionStatus::Created,
        SessionStatus::Running,
        SessionStatus::Paused,
        SessionStatus::Completed,
        SessionStatus::Cancelled,
        SessionStatus::Failed,
    ]
    .into_iter()
    .filter(is_terminal_status)
    .map(|status| status.as_str().to_string())
    .collect()
}

/// The canonical tenant-purge statement for `session_event_archives`.
///
/// Single-sourced here, in the crate that owns the table, because the tenant
/// purge catalog lives in `moa-orchestrator` and a second copy of this string
/// there could drift from the schema without anything noticing: the column name
/// lives only inside a string literal, so no compiler connects the two. The
/// purge catalog references this constant rather than restating it, and the
/// coverage test in this crate runs this exact statement, so the statement that
/// is proven and the statement that is registered cannot diverge.
///
/// Deliberately unqualified: the purge transaction runs it against the
/// deployment `search_path`, and qualifying it here would make the proven form
/// differ from the executed one.
pub const TENANT_PURGE_SQL: &str = "DELETE FROM session_event_archives WHERE tenant_id = $1";

/// One persisted `events` row, captured exactly as it was stored.
///
/// `payload` is the stored JSON, claim-check references intact: the archive
/// copies the row, it does not re-encode the event. Blob bytes stay in
/// `session_blobs`, which retention never touches, so a hydrated event resolves
/// its claim checks the same way a live one does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchivedEvent {
    /// Durable event identifier.
    pub id: Uuid,
    /// Per-session sequence number.
    pub sequence_num: i64,
    /// Stable event-type discriminator.
    pub event_type: String,
    /// Stored payload, including any claim-check references.
    pub payload: serde_json::Value,
    /// Emission timestamp.
    pub timestamp: DateTime<Utc>,
    /// Brain that emitted the event, when one did.
    pub brain_id: Option<Uuid>,
    /// Hand involved in the event, when one was.
    pub hand_id: Option<String>,
    /// Token count attributed to the event.
    pub token_count: Option<i32>,
}

/// Serialized body of one session archive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchiveBody {
    /// Format version of this body.
    pub format_version: i32,
    /// Session whose history this is.
    pub session_id: Uuid,
    /// Full history in ascending sequence order.
    pub events: Vec<ArchivedEvent>,
}

impl ArchiveBody {
    /// Serializes the body into the exact bytes stored in `payload`.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(|error| {
            MoaError::SerializationError(format!("failed to encode session archive: {error}"))
        })
    }

    /// Decodes a body from stored `payload` bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let body: Self = serde_json::from_slice(bytes).map_err(|error| {
            MoaError::SerializationError(format!("failed to decode session archive: {error}"))
        })?;
        if body.format_version != SESSION_ARCHIVE_FORMAT_VERSION {
            return Err(MoaError::StorageError(format!(
                "session archive format version {} is not readable by this build (expected {SESSION_ARCHIVE_FORMAT_VERSION})",
                body.format_version
            )));
        }
        Ok(body)
    }
}

/// Returns the BLAKE3 digest of stored archive bytes.
///
/// Taken over the bytes themselves, never over the source rows, so the digest
/// answers "is what the database is holding still what was written" rather than
/// "did the writer believe it wrote the right thing".
#[must_use]
pub fn archive_digest(bytes: &[u8]) -> [u8; SESSION_ARCHIVE_DIGEST_LEN] {
    *blake3::hash(bytes).as_bytes()
}

/// Metadata describing one stored archive row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEventArchive {
    /// Session whose history was archived.
    pub session_id: SessionId,
    /// Tenant that owns the session.
    pub tenant_id: TenantId,
    /// Format version of the stored body.
    pub format_version: i32,
    /// Number of archived events.
    pub event_count: i64,
    /// Lowest archived sequence number.
    pub first_sequence_num: i64,
    /// Highest archived sequence number.
    pub last_sequence_num: i64,
    /// Size of the stored body in bytes.
    pub payload_bytes: i64,
    /// Digest of the stored body.
    pub content_digest: [u8; SESSION_ARCHIVE_DIGEST_LEN],
    /// When the archive was written.
    pub archived_at: DateTime<Utc>,
}

/// Why a session was not archived.
///
/// These are ordinary outcomes, not faults: a retention pass reports them and
/// moves on. Only genuine storage failures surface as errors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum ArchiveRefusal {
    /// The session can still append to its history.
    NotTerminal {
        /// Status observed under the session row lock.
        status: String,
    },
    /// The session ended more recently than the retention boundary allows.
    WithinRetention {
        /// Boundary the pass was evaluated against.
        boundary: DateTime<Utc>,
        /// When the session reached its terminal state.
        terminal_at: DateTime<Utc>,
    },
    /// An active legal hold covers the tenant or the session's subject.
    LegalHold,
    /// A durable erasure or tenant purge already owns these rows.
    DestructionInFlight,
    /// The session has no events to archive.
    NoEvents,
}

/// Result of one archival attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum ArchiveOutcome {
    /// History was archived and the live rows were deleted.
    Archived(Box<SessionEventArchive>),
    /// The session was already archived by an earlier pass.
    AlreadyArchived,
    /// The session was not eligible.
    Refused(ArchiveRefusal),
}

/// Applies an [`EventRange`] to archived rows before event payload hydration.
///
/// Mirrors the filtering, ordering, and limit semantics of the live
/// `get_events` query. Filtering before claim-check lookup means a caller does
/// not fetch or decode payload blobs for events outside the requested range.
#[must_use]
pub fn apply_archive_range(
    mut events: Vec<ArchivedEvent>,
    range: &EventRange,
) -> Vec<ArchivedEvent> {
    if let Some(event_types) = &range.event_types
        && event_types.is_empty()
    {
        return Vec::new();
    }
    events.retain(|event| {
        if let Some(from_seq) = range.from_seq
            && event.sequence_num < from_seq as i64
        {
            return false;
        }
        if let Some(to_seq) = range.to_seq
            && event.sequence_num > to_seq as i64
        {
            return false;
        }
        if let Some(event_types) = &range.event_types
            && !event_types
                .iter()
                .any(|event_type| event_type.as_str() == event.event_type)
        {
            return false;
        }
        true
    });
    let Some(limit) = range.limit else {
        return events;
    };
    // The live query switches to `ORDER BY sequence_num DESC ... LIMIT` and
    // reverses when a bare limit is asked for with no sequence bounds, which
    // returns the most recent events rather than the oldest.
    let take_from_end = range.from_seq.is_none() && range.to_seq.is_none();
    if events.len() <= limit {
        return events;
    }
    if take_from_end {
        events.split_off(events.len() - limit)
    } else {
        events.truncate(limit);
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moa_core::events::{Event, EventType};

    fn archived_event(sequence_num: i64, event_type: EventType) -> ArchivedEvent {
        ArchivedEvent {
            id: Uuid::from_u128(sequence_num as u128 + 1),
            sequence_num,
            event_type: event_type.as_str().to_string(),
            payload: serde_json::to_value(Event::UserMessage {
                text: format!("event {sequence_num}"),
                attachments: Vec::new(),
            })
            .expect("encode archived test event"),
            timestamp: DateTime::<Utc>::MIN_UTC,
            brain_id: None,
            hand_id: None,
            token_count: None,
        }
    }

    fn sequences(events: &[ArchivedEvent]) -> Vec<i64> {
        events.iter().map(|event| event.sequence_num).collect()
    }

    // Pins: a bare limit on hydrated history returns the NEWEST events, matching
    // the live query's DESC-then-reverse path. Taking the oldest instead would
    // silently truncate an archived session's recent turns during replay.
    #[test]
    fn hydrated_bare_limit_returns_the_newest_events_offline() {
        let events = (0..5)
            .map(|seq| archived_event(seq, EventType::UserMessage))
            .collect();
        let limited = apply_archive_range(events, &EventRange::recent(2));
        assert_eq!(
            sequences(&limited),
            vec![3, 4],
            "a bare limit must return the newest events in ascending order"
        );
    }

    // Pins: once a sequence bound is present the live query orders ASC and takes
    // the FIRST rows, so hydration must not keep taking from the end.
    #[test]
    fn hydrated_bounded_limit_returns_the_oldest_matching_events_offline() {
        let events = (0..5)
            .map(|seq| archived_event(seq, EventType::UserMessage))
            .collect();
        let limited = apply_archive_range(
            events,
            &EventRange {
                from_seq: Some(1),
                to_seq: None,
                event_types: None,
                limit: Some(2),
            },
        );
        assert_eq!(
            sequences(&limited),
            vec![1, 2],
            "a bounded limit must return the oldest matching events"
        );
    }

    // Pins: type and sequence filters compose, and an empty type list selects
    // nothing rather than everything.
    #[test]
    fn hydrated_range_filters_compose_offline() {
        let events = vec![
            archived_event(0, EventType::UserMessage),
            archived_event(1, EventType::BrainResponse),
            archived_event(2, EventType::UserMessage),
            archived_event(3, EventType::BrainResponse),
        ];
        let filtered = apply_archive_range(
            events.clone(),
            &EventRange {
                from_seq: Some(1),
                to_seq: Some(3),
                event_types: Some(vec![EventType::BrainResponse]),
                limit: None,
            },
        );
        assert_eq!(
            sequences(&filtered),
            vec![1, 3],
            "sequence bounds and type filter must both apply"
        );
        let empty = apply_archive_range(
            events,
            &EventRange {
                from_seq: None,
                to_seq: None,
                event_types: Some(Vec::new()),
                limit: None,
            },
        );
        assert!(
            empty.is_empty(),
            "an empty event-type filter must select nothing, got {} records",
            empty.len()
        );
    }

    // Pins: the archive body round-trips through its stored bytes unchanged, and
    // the digest is derived from those exact bytes.
    #[test]
    fn archive_body_round_trips_through_stored_bytes_offline() {
        let body = ArchiveBody {
            format_version: SESSION_ARCHIVE_FORMAT_VERSION,
            session_id: Uuid::from_u128(11),
            events: vec![ArchivedEvent {
                id: Uuid::from_u128(12),
                sequence_num: 0,
                event_type: "user_message".to_string(),
                payload: serde_json::json!({"type": "user_message", "data": {"text": "hi"}}),
                timestamp: DateTime::<Utc>::MIN_UTC,
                brain_id: None,
                hand_id: Some("hand-1".to_string()),
                token_count: Some(3),
            }],
        };
        let bytes = body.to_bytes().expect("encode archive body");
        let decoded = ArchiveBody::from_bytes(&bytes).expect("decode archive body");
        assert_eq!(decoded, body, "archive body must round-trip unchanged");
        assert_eq!(
            archive_digest(&bytes),
            archive_digest(&body.to_bytes().expect("re-encode archive body")),
            "digest must be a function of the encoded bytes alone"
        );
    }

    // Pins: a body written by a future format is refused instead of decoded into
    // a partial history that looks complete.
    #[test]
    fn future_archive_format_is_refused_offline() {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "format_version": SESSION_ARCHIVE_FORMAT_VERSION + 1,
            "session_id": Uuid::from_u128(11),
            "events": [],
        }))
        .expect("encode future archive body");
        let error = ArchiveBody::from_bytes(&bytes).expect_err("future format must be refused");
        assert!(
            error.to_string().contains("not readable by this build"),
            "refusal must name the version mismatch, got: {error}"
        );
    }

    // Pins: the retention scan's status list is exactly the statuses that can no
    // longer append. A status that can still append must never appear here.
    #[test]
    fn terminal_status_list_excludes_appendable_statuses_offline() {
        let terminal = terminal_status_strings();
        assert_eq!(
            terminal,
            vec![
                "completed".to_string(),
                "cancelled".to_string(),
                "failed".to_string()
            ],
            "terminal status list drifted"
        );
        for status in [
            SessionStatus::Created,
            SessionStatus::Running,
            SessionStatus::Paused,
        ] {
            assert!(
                !terminal.contains(&status.as_str().to_string()),
                "{} can still append and must not be archivable",
                status.as_str()
            );
        }
    }
}

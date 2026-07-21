//! Task-segment transition helpers for turn-boundary orchestration.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use moa_config::SegmentBoundaryConfig;
use moa_core::{
    events::Event, types::identifiers::SegmentId, types::identifiers::SessionId,
    types::segments::ActiveSegment, types::segments::SegmentCompletion,
    types::segments::TaskSegment, types::segments::deterministic_segment_id,
};
use serde_json::Value;

use crate::query_rewrite::QueryRewriteResult;

const QUERY_REWRITE_METADATA_KEY: &str = "query_rewrite";

/// Conservative phrase set that deterministically marks the start of a new task
/// when the rewrite LLM produced no boundary signal. Matched case-insensitively
/// at the start of the message or of any clause (after a `.`, `;`, `!`, `?`, or
/// newline). Kept short and precise on purpose: a false boundary mis-scopes a
/// learning unit, which is worse than a missed one, so only unambiguous
/// "new request" phrasings are included.
const NEW_REQUEST_MARKERS: &[&str] = &[
    "new task",
    "next task",
    "separately",
    "unrelated question",
    "different topic",
    "now let's",
    "moving on",
];

/// Deterministic inputs consulted for a segment boundary when the query-rewrite
/// LLM produced no explicit task-boundary signal.
///
/// The tracker only trusts these heuristics when
/// [`QueryRewriteResult::has_boundary_signal`] is absent; an explicit LLM
/// judgment always wins.
#[derive(Debug, Clone, Copy)]
pub struct BoundaryFallbackInput<'a> {
    /// Raw user message text that opened the current turn.
    pub user_message: &'a str,
    /// Timestamp of the newest session event that preceded the current user
    /// message. `None` disables the idle-gap rule (e.g. no prior activity).
    pub previous_event_at: Option<DateTime<Utc>>,
    /// Timestamp of the current user message (turn start), used as the upper
    /// bound of the idle gap.
    pub user_message_at: DateTime<Utc>,
    /// Idle-gap threshold configuration.
    pub config: &'a SegmentBoundaryConfig,
}

/// Segment transition utility used by orchestrators at turn boundaries.
pub struct SegmentTracker;

impl SegmentTracker {
    /// Builds a segment transition from compiled request metadata.
    ///
    /// When the rewrite metadata carries an explicit LLM boundary signal
    /// (`has_boundary_signal`), its `is_new_task` value decides the boundary.
    /// When no signal is present and `fallback` is supplied, deterministic
    /// heuristics (idle gap or an explicit new-request marker) decide instead,
    /// so a session with query rewriting gated or disabled still segments per
    /// task. An explicit `is_new_task = false` from the LLM is never overridden
    /// by the fallback.
    #[must_use]
    pub fn transition_from_metadata(
        metadata: &HashMap<String, Value>,
        session_id: SessionId,
        tenant_id: &str,
        current_segment: &Option<ActiveSegment>,
        now: DateTime<Utc>,
        fallback: Option<BoundaryFallbackInput<'_>>,
    ) -> Option<SegmentTransition> {
        let rewrite = metadata
            .get(QUERY_REWRITE_METADATA_KEY)
            .and_then(|value| serde_json::from_value::<QueryRewriteResult>(value.clone()).ok());

        let should_start = if current_segment.is_none() {
            true
        } else if rewrite
            .as_ref()
            .is_some_and(|rewrite| rewrite.has_boundary_signal)
        {
            rewrite.as_ref().is_some_and(|rewrite| rewrite.is_new_task)
        } else {
            fallback.as_ref().is_some_and(deterministic_new_task)
        };
        if !should_start {
            return None;
        }

        let previous_segment_id = current_segment.as_ref().map(|segment| segment.id);
        let next_index = current_segment
            .as_ref()
            .map(|segment| segment.segment_index.saturating_add(1))
            .unwrap_or(0);
        let task_summary = rewrite.and_then(|rewrite| rewrite.task_summary);
        let segment_id = deterministic_segment_id(session_id, next_index);
        let task_segment = TaskSegment {
            id: segment_id,
            session_id,
            tenant_id: tenant_id.to_string(),
            segment_index: next_index,
            task_summary: task_summary.clone(),
            started_at: now,
            ended_at: None,
            turn_count: 0,
            tools_used: Vec::new(),
            skills_activated: Vec::new(),
            skills_used: Vec::new(),
            token_cost: 0,
            previous_segment_id,
            outcome: None,
            assessment: None,
            outcome_confidence: None,
        };
        let started = SegmentStarted {
            segment_id,
            segment_index: next_index,
            task_summary,
            previous_segment_id,
        };
        let completed = current_segment
            .as_ref()
            .map(|segment| completed_from_active(segment, now));

        Some(SegmentTransition {
            completed,
            started,
            active_segment: task_segment.active_view(),
            task_segment,
        })
    }
}

/// Decides whether deterministic heuristics consider the current message the
/// start of a new task: either a long idle gap since the previous event or an
/// explicit new-request marker at the start of the message or a clause.
fn deterministic_new_task(input: &BoundaryFallbackInput<'_>) -> bool {
    idle_gap_exceeded(input) || starts_with_new_request_marker(input.user_message)
}

/// Returns whether the pause between the previous event and the current user
/// message meets or exceeds the configured idle-gap threshold.
fn idle_gap_exceeded(input: &BoundaryFallbackInput<'_>) -> bool {
    let Some(previous_event_at) = input.previous_event_at else {
        return false;
    };
    let threshold_minutes = i64::try_from(input.config.idle_gap_minutes).unwrap_or(i64::MAX);
    let gap = input
        .user_message_at
        .signed_duration_since(previous_event_at);
    gap >= Duration::minutes(threshold_minutes)
}

/// Returns whether the message opens with a conservative new-request marker,
/// anchored to the message start or a clause start (after sentence-final
/// punctuation or a newline).
fn starts_with_new_request_marker(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower
        .split(['.', ';', '!', '?', '\n'])
        .any(clause_starts_with_marker)
}

/// Returns whether a single clause, after stripping leading non-alphanumeric
/// characters, begins with a marker phrase followed by a word boundary.
fn clause_starts_with_marker(clause: &str) -> bool {
    let trimmed = clause.trim_start_matches(|ch: char| !ch.is_ascii_alphanumeric());
    NEW_REQUEST_MARKERS.iter().any(|marker| {
        trimmed.strip_prefix(marker).is_some_and(|rest| {
            rest.chars()
                .next()
                .is_none_or(|ch| !ch.is_ascii_alphanumeric())
        })
    })
}

/// Segment transition payloads generated for one boundary check.
#[derive(Debug, Clone, PartialEq)]
pub struct SegmentTransition {
    /// Completed segment payload, absent when creating the first segment.
    pub completed: Option<SegmentCompleted>,
    /// Started segment payload.
    pub started: SegmentStarted,
    /// New active segment projection.
    pub active_segment: ActiveSegment,
    /// Full segment row to persist.
    pub task_segment: TaskSegment,
}

/// Payload for a `SegmentStarted` event.
#[derive(Debug, Clone, PartialEq)]
pub struct SegmentStarted {
    /// Segment identifier.
    pub segment_id: SegmentId,
    /// Zero-based segment index.
    pub segment_index: u32,
    /// Optional task summary.
    pub task_summary: Option<String>,
    /// Previous segment identifier.
    pub previous_segment_id: Option<SegmentId>,
}

impl SegmentStarted {
    /// Converts this payload into a durable session event.
    #[must_use]
    pub fn into_event(self) -> Event {
        Event::SegmentStarted {
            segment_id: self.segment_id,
            segment_index: self.segment_index,
            task_summary: self.task_summary,
            previous_segment_id: self.previous_segment_id,
        }
    }
}

/// Payload for a `SegmentCompleted` event.
#[derive(Debug, Clone, PartialEq)]
pub struct SegmentCompleted {
    /// Segment identifier.
    pub segment_id: SegmentId,
    /// Zero-based segment index.
    pub segment_index: u32,
    /// Optional task summary.
    pub task_summary: Option<String>,
    /// Number of turns attributed to the segment.
    pub turn_count: u32,
    /// Tool names used during the segment.
    pub tools_used: Vec<String>,
    /// Skill names injected into the segment's turn manifest.
    pub skills_activated: Vec<String>,
    /// Skill names the model actually engaged during the segment.
    pub skills_used: Vec<String>,
    /// Token cost attributed to the segment.
    pub token_cost: u64,
    /// Segment duration in milliseconds.
    pub duration_ms: u64,
    /// Segment completion update for the store row.
    pub update: SegmentCompletion,
}

impl SegmentCompleted {
    /// Converts this payload into a durable session event.
    #[must_use]
    pub fn into_event(self) -> Event {
        Event::SegmentCompleted {
            segment_id: self.segment_id,
            segment_index: self.segment_index,
            task_summary: self.task_summary,
            turn_count: self.turn_count,
            tools_used: self.tools_used,
            skills_activated: self.skills_activated,
            skills_used: self.skills_used,
            token_cost: self.token_cost,
            duration_ms: self.duration_ms,
        }
    }
}

fn completed_from_active(segment: &ActiveSegment, now: DateTime<Utc>) -> SegmentCompleted {
    let duration_ms = now
        .signed_duration_since(segment.started_at)
        .num_milliseconds()
        .max(0) as u64;
    let update = SegmentCompletion {
        ended_at: now,
        turn_count: segment.turn_count,
        tools_used: segment.tools_used.clone(),
        skills_activated: segment.skills_activated.clone(),
        skills_used: segment.skills_used.clone(),
        token_cost: segment.token_cost,
    };
    SegmentCompleted {
        segment_id: segment.id,
        segment_index: segment.segment_index,
        task_summary: segment.task_summary.clone(),
        turn_count: segment.turn_count,
        tools_used: segment.tools_used.clone(),
        skills_activated: segment.skills_activated.clone(),
        skills_used: segment.skills_used.clone(),
        token_cost: segment.token_cost,
        duration_ms,
        update,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};
    use moa_config::SegmentBoundaryConfig;
    use moa_core::{
        types::identifiers::SessionId, types::segments::ActiveSegment,
        types::segments::deterministic_segment_id,
    };
    use serde_json::json;

    use crate::query_rewrite::{QueryRewriteResult, RewriteReason, RewriteSource};

    use super::{BoundaryFallbackInput, SegmentTracker};

    /// Builds LLM-produced rewrite metadata carrying an authoritative boundary
    /// signal (`has_boundary_signal: true`).
    fn rewrite(is_new_task: bool) -> serde_json::Value {
        serde_json::to_value(QueryRewriteResult {
            retrieval_query: "Update the README".to_string(),
            source: RewriteSource::Rewritten,
            reason: Some(RewriteReason::CoreferenceWithHistory),
            is_new_task,
            has_boundary_signal: true,
            task_summary: Some("Update the README".to_string()),
            task_facets: None,
        })
        .expect("rewrite result should serialize")
    }

    /// Builds a fixed active segment used as the current segment in fallback tests.
    fn active_segment(
        session_id: SessionId,
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> ActiveSegment {
        ActiveSegment {
            id: deterministic_segment_id(session_id, 0),
            segment_index: 0,
            task_summary: Some("Fix failing tests".to_string()),
            started_at,
            tools_used: Vec::new(),
            skills_activated: Vec::new(),
            skills_used: Vec::new(),
            turn_count: 1,
            token_cost: 42,
        }
    }

    #[test]
    fn first_message_creates_segment_zero() {
        let session_id = SessionId::new();
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("query_rewrite".to_string(), rewrite(false));
        let now = chrono::Utc.with_ymd_and_hms(2026, 4, 24, 12, 0, 0).unwrap();

        let transition = SegmentTracker::transition_from_metadata(
            &metadata, session_id, "tenant", &None, now, None,
        )
        .expect("first turn should create a segment");

        assert!(transition.completed.is_none());
        assert_eq!(transition.started.segment_index, 0);
        assert_eq!(transition.task_segment.previous_segment_id, None);
    }

    #[test]
    fn follow_up_does_not_create_transition() {
        let session_id = SessionId::new();
        let started_at = chrono::Utc::now();
        let current = Some(active_segment(session_id, started_at));
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("query_rewrite".to_string(), rewrite(false));

        let transition = SegmentTracker::transition_from_metadata(
            &metadata,
            session_id,
            "tenant",
            &current,
            started_at + Duration::seconds(5),
            None,
        );

        assert!(transition.is_none());
    }

    #[test]
    fn new_task_creates_next_segment_with_previous_id() {
        let session_id = SessionId::new();
        let started_at = chrono::Utc::now();
        let current_id = deterministic_segment_id(session_id, 0);
        let current = Some(ActiveSegment {
            id: current_id,
            segment_index: 0,
            task_summary: Some("Fix failing tests".to_string()),
            started_at,
            tools_used: vec!["bash".to_string()],
            skills_activated: Vec::new(),
            skills_used: Vec::new(),
            turn_count: 2,
            token_cost: 100,
        });
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("query_rewrite".to_string(), rewrite(true));

        let transition = SegmentTracker::transition_from_metadata(
            &metadata,
            session_id,
            "tenant",
            &current,
            started_at + Duration::seconds(5),
            None,
        )
        .expect("new task should transition");

        assert_eq!(transition.started.segment_index, 1);
        assert_eq!(
            transition.task_segment.previous_segment_id,
            Some(current_id)
        );
        assert_eq!(
            transition
                .completed
                .as_ref()
                .map(|completed| completed.tools_used.clone()),
            Some(vec!["bash".to_string()])
        );
    }

    #[test]
    fn malformed_rewrite_metadata_only_creates_first_segment() {
        let session_id = SessionId::new();
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("query_rewrite".to_string(), json!({ "bad": true }));

        assert!(
            SegmentTracker::transition_from_metadata(
                &metadata,
                session_id,
                "tenant",
                &None,
                chrono::Utc::now(),
                None,
            )
            .is_some()
        );
    }

    #[test]
    fn segment_tracker_reads_only_boundary_fields() {
        // Pins: segment creation depends only on boundary fields from query rewrite metadata.
        let session_id = SessionId::new();
        let started_at = chrono::Utc::now();
        let current = Some(active_segment(session_id, started_at));
        let mut metadata = std::collections::HashMap::new();
        metadata.insert(
            "query_rewrite".to_string(),
            json!({
                "retrieval_query": "Write release notes",
                "source": "rewritten",
                "is_new_task": true,
                "has_boundary_signal": true,
                "task_summary": "Write release notes"
            }),
        );

        let transition = SegmentTracker::transition_from_metadata(
            &metadata,
            session_id,
            "tenant",
            &current,
            started_at + Duration::seconds(1),
            None,
        )
        .expect("new task should transition from slim rewrite metadata");

        assert_eq!(
            transition.started.task_summary,
            Some("Write release notes".to_string())
        );
    }

    #[test]
    fn idle_gap_starts_new_segment_when_signal_absent() {
        // Pins: a rewrite-disabled session (no LLM boundary signal) still splits
        // when a long idle gap separates two requests. Without the fallback the
        // session would collapse into one segment forever.
        let session_id = SessionId::new();
        let started_at = chrono::Utc.with_ymd_and_hms(2026, 4, 24, 9, 0, 0).unwrap();
        let current = Some(active_segment(session_id, started_at));
        // No query_rewrite metadata at all: the gate was disabled, so no signal.
        let metadata = std::collections::HashMap::new();
        let config = SegmentBoundaryConfig::default();
        let previous_event_at = started_at + Duration::minutes(2);
        let user_message_at = previous_event_at + Duration::minutes(40);
        let fallback = BoundaryFallbackInput {
            user_message: "Please summarize the quarterly revenue figures",
            previous_event_at: Some(previous_event_at),
            user_message_at,
            config: &config,
        };

        let transition = SegmentTracker::transition_from_metadata(
            &metadata,
            session_id,
            "tenant",
            &current,
            user_message_at,
            Some(fallback),
        )
        .expect("idle gap should start a new segment when no LLM signal is present");

        assert_eq!(transition.started.segment_index, 1);
        assert_eq!(
            transition.task_segment.previous_segment_id,
            Some(deterministic_segment_id(session_id, 0))
        );
    }

    #[test]
    fn explicit_marker_starts_new_segment_when_signal_absent() {
        // Pins: an explicit new-request marker splits the segment even without an
        // idle gap, when the LLM produced no boundary signal.
        let session_id = SessionId::new();
        let started_at = chrono::Utc::now();
        let current = Some(active_segment(session_id, started_at));
        // Original (fail-open) rewrite metadata: has_boundary_signal is false.
        let mut metadata = std::collections::HashMap::new();
        metadata.insert(
            "query_rewrite".to_string(),
            serde_json::to_value(QueryRewriteResult::original(
                "New task: draft the changelog",
            ))
            .expect("original rewrite should serialize"),
        );
        let config = SegmentBoundaryConfig::default();
        let user_message_at = started_at + Duration::seconds(30);
        let fallback = BoundaryFallbackInput {
            user_message: "New task: draft the changelog",
            // Recent activity: no idle gap, so only the marker can trigger.
            previous_event_at: Some(started_at + Duration::seconds(20)),
            user_message_at,
            config: &config,
        };

        let transition = SegmentTracker::transition_from_metadata(
            &metadata,
            session_id,
            "tenant",
            &current,
            user_message_at,
            Some(fallback),
        )
        .expect("explicit marker should start a new segment when no LLM signal is present");

        assert_eq!(transition.started.segment_index, 1);
    }

    #[test]
    fn llm_negative_signal_wins_over_marker() {
        // Pins: an explicit LLM `is_new_task = false` is authoritative; the
        // deterministic marker fallback must not override it.
        let session_id = SessionId::new();
        let started_at = chrono::Utc::now();
        let current = Some(active_segment(session_id, started_at));
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("query_rewrite".to_string(), rewrite(false));
        let config = SegmentBoundaryConfig::default();
        let user_message_at = started_at + Duration::minutes(90);
        let fallback = BoundaryFallbackInput {
            // Both fallback rules would fire (marker present and a long idle gap),
            // but the LLM signal must still win.
            user_message: "New task: unrelated question about billing",
            previous_event_at: Some(started_at),
            user_message_at,
            config: &config,
        };

        let transition = SegmentTracker::transition_from_metadata(
            &metadata,
            session_id,
            "tenant",
            &current,
            user_message_at,
            Some(fallback),
        );

        assert!(
            transition.is_none(),
            "LLM is_new_task=false must suppress the deterministic fallback"
        );
    }

    #[test]
    fn ordinary_continuation_does_not_start_segment_when_signal_absent() {
        // Pins: with no LLM signal, an ordinary follow-up (no marker, short gap)
        // does not spuriously split the segment.
        let session_id = SessionId::new();
        let started_at = chrono::Utc::now();
        let current = Some(active_segment(session_id, started_at));
        let metadata = std::collections::HashMap::new();
        let config = SegmentBoundaryConfig::default();
        let user_message_at = started_at + Duration::minutes(1);
        let fallback = BoundaryFallbackInput {
            user_message: "and also update the tests for that change",
            previous_event_at: Some(started_at + Duration::seconds(40)),
            user_message_at,
            config: &config,
        };

        let transition = SegmentTracker::transition_from_metadata(
            &metadata,
            session_id,
            "tenant",
            &current,
            user_message_at,
            Some(fallback),
        );

        assert!(
            transition.is_none(),
            "ordinary continuation must not create a spurious boundary"
        );
    }
}

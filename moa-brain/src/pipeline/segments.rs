//! Task-segment transition helpers for turn-boundary orchestration.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use moa_core::{
    ActiveSegment, Event, QueryIntent, QueryRewriteResult, Result, SegmentCompletion, SegmentId,
    SessionId, SessionStore, TaskSegment, WorkingContext, deterministic_segment_id,
};
use serde_json::Value;

const QUERY_REWRITE_METADATA_KEY: &str = "query_rewrite";

/// Segment transition utility used by orchestrators at turn boundaries.
pub struct SegmentTracker;

impl SegmentTracker {
    /// Checks context metadata for a task transition after query rewriting.
    pub async fn check_transition(
        ctx: &WorkingContext,
        _session_store: &dyn SessionStore,
        session_id: SessionId,
        tenant_id: &str,
        current_segment: &Option<ActiveSegment>,
    ) -> Result<Option<SegmentTransition>> {
        Ok(Self::transition_from_metadata(
            ctx.metadata(),
            session_id,
            tenant_id,
            current_segment,
            Utc::now(),
        ))
    }

    /// Builds a segment transition from compiled request metadata.
    #[must_use]
    pub fn transition_from_metadata(
        metadata: &HashMap<String, Value>,
        session_id: SessionId,
        tenant_id: &str,
        current_segment: &Option<ActiveSegment>,
        now: DateTime<Utc>,
    ) -> Option<SegmentTransition> {
        let rewrite = metadata
            .get(QUERY_REWRITE_METADATA_KEY)
            .and_then(|value| serde_json::from_value::<QueryRewriteResult>(value.clone()).ok());

        let should_start = current_segment.is_none()
            || rewrite.as_ref().is_some_and(|rewrite| rewrite.is_new_task);
        if !should_start {
            return None;
        }

        let previous_segment_id = current_segment.as_ref().map(|segment| segment.id);
        let next_index = current_segment
            .as_ref()
            .map(|segment| segment.segment_index.saturating_add(1))
            .unwrap_or(0);
        let intent_label = rewrite.as_ref().and_then(intent_label_for_rewrite);
        let task_summary = rewrite.and_then(|rewrite| rewrite.task_summary);
        let segment_id = deterministic_segment_id(session_id, next_index);
        let task_segment = TaskSegment {
            id: segment_id,
            session_id,
            tenant_id: tenant_id.to_string(),
            segment_index: next_index,
            intent_label: intent_label.clone(),
            intent_confidence: None,
            task_summary: task_summary.clone(),
            started_at: now,
            ended_at: None,
            turn_count: 0,
            tools_used: Vec::new(),
            skills_activated: Vec::new(),
            token_cost: 0,
            previous_segment_id,
            resolution: None,
            resolution_signal: None,
            resolution_confidence: None,
        };
        let started = SegmentStarted {
            segment_id,
            segment_index: next_index,
            task_summary,
            intent_label,
            intent_confidence: None,
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
    /// Optional intent label.
    pub intent_label: Option<String>,
    /// Optional intent confidence.
    pub intent_confidence: Option<f64>,
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
            intent_label: self.intent_label,
            intent_confidence: self.intent_confidence,
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
    /// Optional intent label.
    pub intent_label: Option<String>,
    /// Optional task summary.
    pub task_summary: Option<String>,
    /// Number of turns attributed to the segment.
    pub turn_count: u32,
    /// Tool names used during the segment.
    pub tools_used: Vec<String>,
    /// Skill names activated during the segment.
    pub skills_activated: Vec<String>,
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
            intent_label: self.intent_label,
            task_summary: self.task_summary,
            turn_count: self.turn_count,
            tools_used: self.tools_used,
            skills_activated: self.skills_activated,
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
        token_cost: segment.token_cost,
    };
    SegmentCompleted {
        segment_id: segment.id,
        segment_index: segment.segment_index,
        intent_label: segment.intent_label.clone(),
        task_summary: segment.task_summary.clone(),
        turn_count: segment.turn_count,
        tools_used: segment.tools_used.clone(),
        skills_activated: segment.skills_activated.clone(),
        token_cost: segment.token_cost,
        duration_ms,
        update,
    }
}

fn intent_label_for_rewrite(rewrite: &QueryRewriteResult) -> Option<String> {
    match &rewrite.intent {
        QueryIntent::Coding => Some("coding"),
        QueryIntent::Research => Some("research"),
        QueryIntent::FileOperation => Some("file_operation"),
        QueryIntent::SystemAdmin => Some("system_admin"),
        QueryIntent::Creative => Some("creative"),
        QueryIntent::Question => Some("question"),
        QueryIntent::Conversation => Some("conversation"),
        QueryIntent::Unknown => None,
    }
    .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};
    use moa_core::{
        ActiveSegment, QueryIntent, QueryRewriteResult, RewriteSource, SessionId,
        deterministic_segment_id,
    };
    use serde_json::json;

    use super::SegmentTracker;

    fn rewrite(is_new_task: bool) -> serde_json::Value {
        serde_json::to_value(QueryRewriteResult {
            rewritten_query: "Update the README".to_string(),
            intent: QueryIntent::FileOperation,
            sub_queries: Vec::new(),
            suggested_tools: Vec::new(),
            needs_clarification: false,
            clarification_question: None,
            is_new_task,
            task_summary: Some("Update the README".to_string()),
            source: RewriteSource::Rewritten,
        })
        .expect("rewrite result should serialize")
    }

    #[test]
    fn first_message_creates_segment_zero() {
        let session_id = SessionId::new();
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("query_rewrite".to_string(), rewrite(false));
        let now = chrono::Utc.with_ymd_and_hms(2026, 4, 24, 12, 0, 0).unwrap();

        let transition =
            SegmentTracker::transition_from_metadata(&metadata, session_id, "tenant", &None, now)
                .expect("first turn should create a segment");

        assert!(transition.completed.is_none());
        assert_eq!(transition.started.segment_index, 0);
        assert_eq!(transition.task_segment.previous_segment_id, None);
    }

    #[test]
    fn follow_up_does_not_create_transition() {
        let session_id = SessionId::new();
        let started_at = chrono::Utc::now();
        let current = Some(ActiveSegment {
            id: deterministic_segment_id(session_id, 0),
            segment_index: 0,
            intent_label: Some("coding".to_string()),
            task_summary: Some("Fix failing tests".to_string()),
            started_at,
            tools_used: Vec::new(),
            skills_activated: Vec::new(),
            turn_count: 1,
            token_cost: 42,
        });
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("query_rewrite".to_string(), rewrite(false));

        let transition = SegmentTracker::transition_from_metadata(
            &metadata,
            session_id,
            "tenant",
            &current,
            started_at + Duration::seconds(5),
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
            intent_label: Some("coding".to_string()),
            task_summary: Some("Fix failing tests".to_string()),
            started_at,
            tools_used: vec!["bash".to_string()],
            skills_activated: Vec::new(),
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
            )
            .is_some()
        );
    }
}

//! Pure helpers for assessing task segments during turn execution.

use chrono::{DateTime, Utc};
use moa_core::{
    config::ResolutionConfig, events::Event, types::events_stream::EventRecord,
    types::identifiers::SegmentId, types::segment_assessment::AssessmentPhase,
    types::segment_assessment::SegmentAssessment, types::segment_assessment::SegmentBaseline,
    types::segments::ActiveSegment, types::segments::TaskSegment, types::session::SessionMeta,
};

use crate::pipeline::segments::SegmentCompleted;
use crate::segment_assessment::{
    AssessmentOverride, SegmentAssessor, continuation_signal, self_assessment_signal,
    structural_signal, tool_signal, verification_signal,
};

/// Durable sequence numbers that bound a segment in the session event log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentBoundarySequences {
    /// Sequence number for the segment-start event.
    pub start_seq: u64,
    /// Sequence number for the segment-completion event, when the segment is complete.
    pub completed_seq: Option<u64>,
}

/// Finds the start and completion sequence numbers for the requested segment.
#[must_use]
pub fn segment_boundary_sequences(
    boundary_events: &[EventRecord],
    segment_id: SegmentId,
) -> Option<SegmentBoundarySequences> {
    let mut start_seq = None;
    let mut completed_seq = None;
    for record in boundary_events {
        match &record.event {
            Event::SegmentStarted {
                segment_id: started_id,
                ..
            } if *started_id == segment_id && start_seq.is_none() => {
                start_seq = Some(record.sequence_num);
            }
            Event::SegmentCompleted {
                segment_id: completed_id,
                ..
            } if *completed_id == segment_id && completed_seq.is_none() => {
                completed_seq = Some(record.sequence_num);
            }
            _ => {}
        }
    }

    start_seq.map(|start_seq| SegmentBoundarySequences {
        start_seq,
        completed_seq,
    })
}

/// Returns the inclusive upper sequence number for bounded assessment loading.
#[must_use]
pub fn segment_assessment_to_seq(
    boundary: SegmentBoundarySequences,
    cutoff_before_seq: Option<u64>,
    stop_at_completion: bool,
) -> Option<u64> {
    if let Some(sequence_num) = cutoff_before_seq {
        return Some(sequence_num.saturating_sub(1));
    }
    if stop_at_completion {
        return boundary.completed_seq;
    }
    None
}

/// Filters session events down to the portion that should inform a segment assessment.
#[must_use]
pub fn segment_events_for_assessment(
    events: &[EventRecord],
    segment_id: SegmentId,
    cutoff_before_seq: Option<u64>,
) -> Vec<EventRecord> {
    let start_seq = events.iter().find_map(|record| match &record.event {
        Event::SegmentStarted {
            segment_id: started_id,
            ..
        } if *started_id == segment_id => Some(record.sequence_num),
        _ => None,
    });
    let completed_seq = events.iter().find_map(|record| match &record.event {
        Event::SegmentCompleted {
            segment_id: completed_id,
            ..
        } if *completed_id == segment_id => Some(record.sequence_num),
        _ => None,
    });
    let end_exclusive = cutoff_before_seq
        .or_else(|| completed_seq.map(|sequence_num| sequence_num.saturating_add(1)));

    events
        .iter()
        .filter(|record| start_seq.is_none_or(|start_seq| record.sequence_num >= start_seq))
        .filter(|record| end_exclusive.is_none_or(|end_seq| record.sequence_num < end_seq))
        .cloned()
        .collect()
}

/// Returns the latest user message and its sequence number.
#[must_use]
pub fn latest_user_message(events: &[EventRecord]) -> Option<(&str, u64)> {
    events.iter().rev().find_map(|record| match &record.event {
        Event::UserMessage { text, .. } => Some((text.as_str(), record.sequence_num)),
        _ => None,
    })
}

/// Converts a completed segment transition into the persisted segment projection.
#[must_use]
pub fn task_segment_from_completed(
    meta: &SessionMeta,
    completed: &SegmentCompleted,
    events: &[EventRecord],
    assessment: &SegmentAssessment,
) -> TaskSegment {
    let (started_at, previous_segment_id) = events
        .iter()
        .find_map(|record| match &record.event {
            Event::SegmentStarted {
                segment_id,
                previous_segment_id,
                ..
            } if *segment_id == completed.segment_id => {
                Some((record.timestamp, *previous_segment_id))
            }
            _ => None,
        })
        .unwrap_or((assessment.assessed_at, None));
    TaskSegment {
        id: completed.segment_id,
        session_id: meta.id,
        tenant_id: meta.tenant_id.to_string(),
        segment_index: completed.segment_index,
        task_summary: completed.task_summary.clone(),
        started_at,
        ended_at: Some(assessment.assessed_at),
        turn_count: completed.turn_count,
        tools_used: completed.tools_used.clone(),
        skills_activated: completed.skills_activated.clone(),
        token_cost: completed.token_cost,
        previous_segment_id,
        outcome: Some(assessment.outcome.as_str().to_string()),
        assessment: Some(assessment.clone()),
        outcome_confidence: Some(assessment.confidence),
    }
}

/// Converts an active segment projection into the persisted segment projection.
#[must_use]
pub fn task_segment_from_active(
    meta: &SessionMeta,
    segment: &ActiveSegment,
    assessment: &SegmentAssessment,
    ended_at: Option<DateTime<Utc>>,
) -> TaskSegment {
    TaskSegment {
        id: segment.id,
        session_id: meta.id,
        tenant_id: meta.tenant_id.to_string(),
        segment_index: segment.segment_index,
        task_summary: segment.task_summary.clone(),
        started_at: segment.started_at,
        ended_at,
        turn_count: segment.turn_count,
        tools_used: segment.tools_used.clone(),
        skills_activated: segment.skills_activated.clone(),
        token_cost: segment.token_cost,
        previous_segment_id: None,
        outcome: Some(assessment.outcome.as_str().to_string()),
        assessment: Some(assessment.clone()),
        outcome_confidence: Some(assessment.confidence),
    }
}

/// Produces a composite assessment for a segment from event-derived signals.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn assess_segment_events(
    segment_events: &[EventRecord],
    turn_count: u32,
    token_cost: u64,
    duration_ms: u64,
    baseline: Option<&SegmentBaseline>,
    next_user_message: Option<&str>,
    is_new_task: bool,
    assessed_at: chrono::DateTime<chrono::Utc>,
    phase: AssessmentPhase,
    extra_overrides: &[AssessmentOverride],
    config: &ResolutionConfig,
) -> SegmentAssessment {
    let tool = tool_signal::score(segment_events);
    let verification = verification_signal::score(segment_events);
    let continuation = continuation_signal::score(
        continuation_signal::ContinuationInput {
            next_user_message,
            initial_query: first_user_message(segment_events),
            is_new_task,
        },
        config.rephrase_similarity_threshold,
    );
    let self_assessment = self_assessment_signal::score(last_brain_response(segment_events));
    let structural = structural_signal::score(
        structural_signal::SegmentMetrics {
            turn_count,
            token_cost,
            duration_secs: duration_ms as f64 / 1_000.0,
        },
        baseline,
        config.structural_min_samples,
    );
    let mut overrides = extra_overrides.to_vec();
    if let Some(override_value) = verification_signal::override_for_events(segment_events) {
        overrides.push(override_value);
    }
    if tool_signal::all_tools_failed(segment_events) {
        overrides.push(AssessmentOverride::AllToolsFailed);
    }

    SegmentAssessor::new(config.weights).assess(
        tool,
        verification,
        continuation,
        self_assessment,
        structural,
        assessed_at,
        phase,
        &overrides,
    )
}

fn first_user_message(events: &[EventRecord]) -> Option<&str> {
    events.iter().find_map(|record| match &record.event {
        Event::UserMessage { text, .. } => Some(text.as_str()),
        _ => None,
    })
}

fn last_brain_response(events: &[EventRecord]) -> Option<&str> {
    events.iter().rev().find_map(|record| match &record.event {
        Event::BrainResponse { text, .. } => Some(text.as_str()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use moa_core::{
        config::ResolutionConfig, events::Event, types::events_stream::EventRecord,
        types::identifiers::SegmentId, types::identifiers::SessionId,
        types::segment_assessment::AssessmentPhase,
    };
    use uuid::Uuid;

    use super::{
        SegmentBoundarySequences, assess_segment_events, segment_assessment_to_seq,
        segment_boundary_sequences, segment_events_for_assessment,
    };

    #[test]
    fn assessment_preserves_caller_timestamp() {
        let assessed_at = Utc
            .with_ymd_and_hms(2026, 7, 11, 0, 0, 0)
            .single()
            .expect("fixed assessment timestamp must be valid");
        let assessment = assess_segment_events(
            &[],
            0,
            0,
            0,
            None,
            None,
            false,
            assessed_at,
            AssessmentPhase::Immediate,
            &[],
            &ResolutionConfig::default(),
        );

        assert_eq!(assessment.assessed_at, assessed_at);
    }

    fn event_record(session_id: SessionId, sequence_num: u64, event: Event) -> EventRecord {
        let event_type = event.event_type();
        EventRecord {
            id: Uuid::from_u128(sequence_num as u128),
            session_id,
            sequence_num,
            event_type,
            event,
            timestamp: Utc::now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        }
    }

    fn segment_started(
        session_id: SessionId,
        sequence_num: u64,
        segment_id: SegmentId,
    ) -> EventRecord {
        event_record(
            session_id,
            sequence_num,
            Event::SegmentStarted {
                segment_id,
                segment_index: 0,
                task_summary: Some("target task".to_string()),
                previous_segment_id: None,
            },
        )
    }

    fn segment_completed(
        session_id: SessionId,
        sequence_num: u64,
        segment_id: SegmentId,
    ) -> EventRecord {
        event_record(
            session_id,
            sequence_num,
            Event::SegmentCompleted {
                segment_id,
                segment_index: 0,
                task_summary: Some("target task".to_string()),
                turn_count: 1,
                tools_used: Vec::new(),
                skills_activated: Vec::new(),
                token_cost: 10,
                duration_ms: 50,
            },
        )
    }

    fn user_message(session_id: SessionId, sequence_num: u64, text: &str) -> EventRecord {
        event_record(
            session_id,
            sequence_num,
            Event::UserMessage {
                text: text.to_string(),
                attachments: Vec::new(),
            },
        )
    }

    fn warning(session_id: SessionId, sequence_num: u64, message: &str) -> EventRecord {
        event_record(
            session_id,
            sequence_num,
            Event::Warning {
                message: message.to_string(),
            },
        )
    }

    #[test]
    fn segment_boundary_sequences_match_requested_segment_only() {
        // Pins: boundary lookup uses durable segment boundary events for the target segment.
        let session_id = SessionId::new();
        let target_segment = SegmentId::new();
        let other_segment = SegmentId::new();
        let boundaries = vec![
            segment_started(session_id, 2, other_segment),
            segment_started(session_id, 10, target_segment),
            segment_completed(session_id, 18, other_segment),
            segment_completed(session_id, 31, target_segment),
        ];

        assert_eq!(
            segment_boundary_sequences(&boundaries, target_segment),
            Some(SegmentBoundarySequences {
                start_seq: 10,
                completed_seq: Some(31),
            })
        );
        assert_eq!(
            segment_boundary_sequences(&boundaries, SegmentId::new()),
            None
        );
    }

    #[test]
    fn segment_assessment_to_seq_prefers_next_user_cutoff() {
        // Pins: completed segment assessment ends before the next user message when known.
        let boundary = SegmentBoundarySequences {
            start_seq: 10,
            completed_seq: Some(40),
        };

        assert_eq!(
            segment_assessment_to_seq(boundary, Some(35), true),
            Some(34)
        );
        assert_eq!(segment_assessment_to_seq(boundary, None, true), Some(40));
        assert_eq!(segment_assessment_to_seq(boundary, None, false), None);
    }

    #[test]
    fn segment_events_for_assessment_starts_at_segment_and_stops_before_cutoff() {
        // Pins: segment assessment excludes prior events and the next task's user message.
        let session_id = SessionId::new();
        let target_segment = SegmentId::new();
        let events = vec![
            user_message(session_id, 1, "previous task"),
            segment_started(session_id, 2, target_segment),
            user_message(session_id, 3, "target task"),
            warning(session_id, 4, "inside target segment"),
            user_message(session_id, 5, "next task"),
            segment_completed(session_id, 6, target_segment),
            warning(session_id, 7, "after target segment"),
        ];

        let filtered = segment_events_for_assessment(&events, target_segment, Some(5));
        let sequences = filtered
            .iter()
            .map(|record| record.sequence_num)
            .collect::<Vec<_>>();

        assert_eq!(sequences, vec![2, 3, 4]);
    }
}

//! Out-of-line tests for the real local contradiction detector.

use chrono::{Duration, Utc};
use moa_memory_graph::{NodeIndexRow, NodeLabel, PiiClass};
use moa_memory_ingest::{Conflict, RrfPlusJudgeDetector};
use serde_json::{Value, json};
use uuid::Uuid;

fn candidate(summary: &str) -> NodeIndexRow {
    candidate_with(summary, None, None)
}

fn candidate_with(
    summary: &str,
    valid_to: Option<chrono::DateTime<Utc>>,
    extra: Option<Value>,
) -> NodeIndexRow {
    let mut words = summary.split_whitespace();
    let subject = words.next().unwrap_or("fact");
    let predicate = words.next().unwrap_or("states");
    let object = words.collect::<Vec<_>>().join(" ");
    let mut properties = json!({
        "summary": summary,
        "subject": subject,
        "predicate": predicate,
        "object": object,
    });
    if let (Some(map), Some(extra)) = (properties.as_object_mut(), extra)
        && let Some(extra_map) = extra.as_object()
    {
        map.extend(extra_map.clone());
    }
    NodeIndexRow {
        uid: Uuid::now_v7(),
        label: NodeLabel::Fact,
        workspace_id: Some(Uuid::now_v7().to_string()),
        user_id: None,
        scope: "workspace".to_string(),
        name: summary.to_string(),
        pii_class: PiiClass::None,
        valid_to,
        valid_from: Utc::now() - Duration::days(1),
        properties_summary: Some(properties),
        last_accessed_at: Utc::now(),
        quality_score: 0.5,
    }
}

/// Tampers with the object for the same subject and predicate: port 3000 becomes 8080.
#[tokio::test]
async fn contradiction_detector_flags_two_facts_with_same_subject_predicate_different_object() {
    let candidate = candidate("API runs_on_port 3000");
    let detector = RrfPlusJudgeDetector::default();

    let conflict = detector
        .judge_candidates("API runs_on_port 8080", std::slice::from_ref(&candidate))
        .await
        .expect("real judge should compare structured facts");

    assert_eq!(conflict, Conflict::Supersede(candidate.uid));
}

/// Changes the subject while keeping the predicate/object identical.
#[tokio::test]
async fn contradiction_detector_does_not_flag_two_facts_with_different_subjects() {
    let candidate = candidate("Worker runs_on_port 3000");
    let detector = RrfPlusJudgeDetector::default();

    let conflict = detector
        .judge_candidates("API runs_on_port 3000", &[candidate])
        .await
        .expect("real judge should ignore different subjects");

    assert_eq!(conflict, Conflict::Insert);
}

/// Changes the predicate while keeping the subject/object compatible.
#[tokio::test]
async fn contradiction_detector_does_not_flag_two_facts_with_different_predicates() {
    let candidate = candidate("API uses_protocol HTTPS");
    let detector = RrfPlusJudgeDetector::default();

    let conflict = detector
        .judge_candidates("API runs_on_port 3000", &[candidate])
        .await
        .expect("real judge should ignore different predicates");

    assert_eq!(conflict, Conflict::Insert);
}

/// Closes the old candidate before judging the newer replacement fact.
#[tokio::test]
async fn contradiction_detector_handles_temporal_facts_correctly_when_ranges_overlap() {
    let candidate = candidate_with(
        "API runs_on_port 3000",
        Some(Utc::now() - Duration::hours(1)),
        None,
    );
    let detector = RrfPlusJudgeDetector::default();

    let conflict = detector
        .judge_candidates("API runs_on_port 8080", &[candidate])
        .await
        .expect("closed candidates should be ignored");

    assert_eq!(conflict, Conflict::Insert);
}

/// Keeps the old candidate active so the changed object overlaps in application time.
#[tokio::test]
async fn contradiction_detector_handles_temporal_facts_correctly_when_ranges_overlap_partially() {
    let candidate = candidate("API runs_on_port 3000");
    let detector = RrfPlusJudgeDetector::default();

    let conflict = detector
        .judge_candidates("API runs_on_port 8080", std::slice::from_ref(&candidate))
        .await
        .expect("active candidate should be considered overlapping");

    assert_eq!(conflict, Conflict::Supersede(candidate.uid));
}

/// Adds evidence metadata and verifies the detector still surfaces the concrete conflicting uid.
#[tokio::test]
async fn contradiction_detector_uses_evidence_score_to_resolve_conflicts_when_both_facts_recent() {
    let candidate = candidate_with(
        "API runs_on_port 3000",
        None,
        Some(json!({ "evidence_score": 0.20 })),
    );
    let detector = RrfPlusJudgeDetector::default();

    let conflict = detector
        .judge_candidates("API runs_on_port 8080", std::slice::from_ref(&candidate))
        .await
        .expect("recent conflicting facts should return a concrete supersession target");

    assert_eq!(conflict, Conflict::Supersede(candidate.uid));
}

/// Repeats the exact fact instead of changing a field.
#[tokio::test]
async fn contradiction_detector_does_not_flag_self_referential_fact_repetition() {
    let candidate = candidate("API runs_on_port 3000");
    let detector = RrfPlusJudgeDetector::default();

    let conflict = detector
        .judge_candidates("API runs_on_port 3000", std::slice::from_ref(&candidate))
        .await
        .expect("same fact should deduplicate");

    assert_eq!(conflict, Conflict::Duplicate(candidate.uid));
}

/// Supplies no candidate facts.
#[tokio::test]
async fn contradiction_detector_returns_empty_when_input_set_is_empty() {
    let detector = RrfPlusJudgeDetector::default();

    let conflict = detector
        .judge_candidates("API runs_on_port 3000", &[])
        .await
        .expect("empty candidate set should not fail");

    assert_eq!(conflict, Conflict::Insert);
}

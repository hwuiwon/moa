//! Postgres-backed behavior tests for weakness-mining candidate filing.
//!
//! Drives `mine_and_file_session_failures` — the production path between
//! session failure signals and reviewable learning candidates — against an
//! isolated database.

use chrono::Utc;
use moa_core::{
    Event, EventRecord, LearningCandidateStatus, LearningCandidateStatusUpdate,
    LearningCandidateType, SessionId, TenantId, ToolCallId,
};
use moa_skills::mining::mine_and_file_session_failures;
use moa_test_support::postgres::bootstrap_test_db;
use uuid::Uuid;

#[tokio::test]
async fn recurring_durable_tool_errors_file_one_reviewable_candidate_db() {
    // Pins: three durable failures of the same tool cross the recurrence threshold and
    // file exactly one Proposed skill candidate carrying the pattern evidence.
    let test_db = bootstrap_test_db().await.expect("bootstrap mining test db");
    let tenant_id = TenantId::new();
    let events = durable_errors(SessionId::new(), "bash", 3);

    let applied = mine_and_file_session_failures(test_db.store(), tenant_id, &events, Utc::now())
        .await
        .expect("mine recurring failures");

    assert_eq!(applied, 1);
    let candidates = open_candidates(&test_db, tenant_id).await;
    assert_eq!(candidates.len(), 1);
    let candidate = &candidates[0];
    assert_eq!(candidate.candidate_type, LearningCandidateType::Skill);
    assert_eq!(candidate.status, LearningCandidateStatus::Proposed);
    assert_eq!(candidate.payload["kind"], "weakness_mining_pattern");
    assert_eq!(candidate.payload["pattern_key"], "durable_tool_error:bash");
    assert_eq!(candidate.payload["pattern_occurrences"], 3);
    assert_eq!(
        candidate.payload["evidence"]
            .as_array()
            .expect("evidence refs recorded")
            .len(),
        3
    );
}

#[tokio::test]
async fn below_threshold_failures_file_nothing_db() {
    // Pins: two occurrences stay below the recurrence threshold — no review noise.
    let test_db = bootstrap_test_db().await.expect("bootstrap mining test db");
    let tenant_id = TenantId::new();
    let events = durable_errors(SessionId::new(), "bash", 2);

    let applied = mine_and_file_session_failures(test_db.store(), tenant_id, &events, Utc::now())
        .await
        .expect("mine below-threshold failures");

    assert_eq!(applied, 0);
    assert!(open_candidates(&test_db, tenant_id).await.is_empty());
}

#[tokio::test]
async fn remining_bumps_open_candidate_instead_of_duplicating_db() {
    // Pins: a later session re-observing the same pattern bumps the open candidate's
    // occurrence evidence instead of filing a second review item.
    let test_db = bootstrap_test_db().await.expect("bootstrap mining test db");
    let tenant_id = TenantId::new();

    mine_and_file_session_failures(
        test_db.store(),
        tenant_id,
        &durable_errors(SessionId::new(), "bash", 3),
        Utc::now(),
    )
    .await
    .expect("first mining pass");
    let applied = mine_and_file_session_failures(
        test_db.store(),
        tenant_id,
        &durable_errors(SessionId::new(), "bash", 4),
        Utc::now(),
    )
    .await
    .expect("second mining pass");

    assert_eq!(applied, 1, "re-observation bumps the open candidate");
    let candidates = open_candidates(&test_db, tenant_id).await;
    assert_eq!(candidates.len(), 1, "no duplicate candidate rows");
    let evaluation = candidates[0]
        .evaluation_payload
        .as_ref()
        .expect("bump records fresh evidence");
    assert_eq!(evaluation["occurrence_count"], 4);
}

#[tokio::test]
async fn claimed_candidate_keeps_review_state_on_remine_db() {
    // Pins: a candidate a reviewer already claimed (Evaluating) is not reverted to
    // Proposed by a background mining bump.
    let test_db = bootstrap_test_db().await.expect("bootstrap mining test db");
    let tenant_id = TenantId::new();

    mine_and_file_session_failures(
        test_db.store(),
        tenant_id,
        &durable_errors(SessionId::new(), "bash", 3),
        Utc::now(),
    )
    .await
    .expect("first mining pass");
    let candidate_id = open_candidates(&test_db, tenant_id).await[0].id;
    let claimed = test_db
        .store()
        .update_learning_candidate_status_from(
            &LearningCandidateStatusUpdate {
                candidate_id,
                status: LearningCandidateStatus::Evaluating,
                status_reason: Some("claimed by reviewer".to_string()),
                evaluation_payload: None,
                updated_at: Utc::now(),
            },
            LearningCandidateStatus::Proposed,
        )
        .await
        .expect("claim mined candidate");
    assert!(claimed);

    let applied = mine_and_file_session_failures(
        test_db.store(),
        tenant_id,
        &durable_errors(SessionId::new(), "bash", 5),
        Utc::now(),
    )
    .await
    .expect("re-mine while claimed");

    assert_eq!(applied, 0, "claimed candidates are never bumped");
    let reloaded = test_db
        .store()
        .get_learning_candidate(&tenant_id, candidate_id)
        .await
        .expect("reload claimed candidate")
        .expect("candidate exists");
    assert_eq!(reloaded.status, LearningCandidateStatus::Evaluating);
    assert_eq!(
        reloaded.status_reason.as_deref(),
        Some("claimed by reviewer")
    );
}

fn durable_errors(session_id: SessionId, tool_name: &str, count: usize) -> Vec<EventRecord> {
    (0..count)
        .map(|index| {
            let event = Event::ToolError {
                tool_id: ToolCallId::new(),
                provider_tool_use_id: None,
                tool_name: tool_name.to_string(),
                error: "terminal failure".to_string(),
                retryable: false,
            };
            EventRecord {
                id: Uuid::now_v7(),
                session_id,
                sequence_num: index as u64 + 1,
                event_type: event.event_type(),
                event,
                timestamp: Utc::now(),
                brain_id: None,
                hand_id: None,
                token_count: None,
            }
        })
        .collect()
}

async fn open_candidates(
    test_db: &moa_test_support::postgres::TestDb,
    tenant_id: TenantId,
) -> Vec<moa_core::LearningCandidate> {
    test_db
        .store()
        .list_learning_candidates(
            &tenant_id.to_string(),
            Some(LearningCandidateStatus::Proposed),
            50,
        )
        .await
        .expect("list open candidates")
}

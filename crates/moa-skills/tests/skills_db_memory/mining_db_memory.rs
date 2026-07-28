//! Postgres-backed behavior tests for weakness-mining candidate filing.
//!
//! Drives `mine_and_file_session_failures` — the production path between
//! session failure signals and reviewable learning candidates — against an
//! isolated database.

use moa_core::{
    events::Event, types::events_stream::EventRecord, types::experience::LearningCandidateStatus,
    types::experience::LearningCandidateStatusUpdate, types::experience::LearningCandidateType,
    types::experience::LearningProposalKind, types::identifiers::TenantId,
    types::identifiers::ToolCallId,
};
use moa_skills::mining::mine_and_file_session_failures;
use moa_test_support::postgres::bootstrap_test_db;
use uuid::Uuid;

#[tokio::test]
async fn recurring_durable_tool_errors_file_one_authoring_candidate_db() {
    // Pins: three durable failures of the same tool cross the recurrence threshold and
    // file exactly one AUTHORING item carrying the pattern evidence.
    //
    // Mining observes that something keeps failing; it does not produce a change any
    // code can apply. Filing it as `Proposed` would put it on the review queue beside
    // skill drafts a reviewer can actually accept — a review contract the system
    // cannot keep — so it is `SkillAuthoring`/`NeedsAuthoring` and its only exit is
    // dismissal.
    let test_db = bootstrap_test_db().await.expect("bootstrap mining test db");
    let tenant_id = TenantId::new();
    let events = durable_errors(&test_db, tenant_id, "bash", 3).await;

    let applied = mine_and_file_session_failures(
        test_db.store(),
        tenant_id,
        &events,
        moa_test_support::fixtures::pg_now(),
    )
    .await
    .expect("mine recurring failures");

    assert_eq!(applied, 1);
    let candidates = open_candidates(&test_db, tenant_id).await;
    assert_eq!(candidates.len(), 1);
    let candidate = &candidates[0];
    assert_eq!(candidate.candidate_type, LearningCandidateType::Skill);
    assert_eq!(candidate.status, LearningCandidateStatus::NeedsAuthoring);
    assert_eq!(
        candidate.proposal_kind,
        LearningProposalKind::SkillAuthoring
    );
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
    let events = durable_errors(&test_db, tenant_id, "bash", 2).await;

    let applied = mine_and_file_session_failures(
        test_db.store(),
        tenant_id,
        &events,
        moa_test_support::fixtures::pg_now(),
    )
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
        &durable_errors(&test_db, tenant_id, "bash", 3).await,
        moa_test_support::fixtures::pg_now(),
    )
    .await
    .expect("first mining pass");
    let applied = mine_and_file_session_failures(
        test_db.store(),
        tenant_id,
        &durable_errors(&test_db, tenant_id, "bash", 4).await,
        moa_test_support::fixtures::pg_now(),
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
async fn a_dismissed_pattern_keeps_its_dismissal_when_the_failure_recurs_db() {
    // Pins the boundary of what dismissal protects, as production actually behaves.
    //
    // A mined candidate's id is DETERMINISTIC in the tenant and pattern key, so a later
    // pass over the same failure re-upserts the same row rather than creating a second
    // one. The upsert deliberately omits `status` from its update set, so the reviewer's
    // DISMISSAL SURVIVES — that is the load-bearing guarantee, and the reason the mining
    // path can run unattended.
    //
    // Two things it does NOT protect, pinned here so a future change to either is
    // visible rather than discovered: the reviewer's `status_reason` is overwritten by
    // the pass's own description, and the pass reports the row as applied even though
    // nothing reopened. Both are recorded as current behavior, not endorsed as design.
    //
    // (This replaces a test that claimed a mined candidate into `Evaluating`. That
    // state is no longer reachable for an authoring kind — the database refuses the
    // transition — so the old test asserted a shape production cannot produce.)
    let test_db = bootstrap_test_db().await.expect("bootstrap mining test db");
    let tenant_id = TenantId::new();

    mine_and_file_session_failures(
        test_db.store(),
        tenant_id,
        &durable_errors(&test_db, tenant_id, "bash", 3).await,
        moa_test_support::fixtures::pg_now(),
    )
    .await
    .expect("first mining pass");
    let candidate_id = open_candidates(&test_db, tenant_id).await[0].id;
    let dismissed = test_db
        .store()
        .update_learning_candidate_status_from(
            &LearningCandidateStatusUpdate {
                candidate_id,
                status: LearningCandidateStatus::Dismissed,
                status_reason: Some("not worth authoring".to_string()),
                evaluation_payload: None,
                updated_at: moa_test_support::fixtures::pg_now(),
            },
            LearningCandidateStatus::NeedsAuthoring,
        )
        .await
        .expect("dismiss mined candidate");
    assert!(dismissed);

    let applied = mine_and_file_session_failures(
        test_db.store(),
        tenant_id,
        &durable_errors(&test_db, tenant_id, "bash", 5).await,
        moa_test_support::fixtures::pg_now(),
    )
    .await
    .expect("re-mine after dismissal");

    assert_eq!(
        applied, 1,
        "the recurring failure is re-upserted and counted, though nothing reopens"
    );
    let reloaded = test_db
        .store()
        .get_learning_candidate(&tenant_id, candidate_id)
        .await
        .expect("reload dismissed candidate")
        .expect("candidate exists");
    assert_eq!(
        reloaded.status,
        LearningCandidateStatus::Dismissed,
        "a background pass must never reopen what a reviewer closed"
    );
    assert!(
        open_candidates(&test_db, tenant_id).await.is_empty(),
        "nothing returns to the authoring queue"
    );
    assert_ne!(
        reloaded.status_reason.as_deref(),
        Some("not worth authoring"),
        "current behavior: the pass overwrites the reviewer's reason with its own \
         description, so the dismissal survives but the rationale for it does not"
    );
}

/// Persists a session and `count` durable tool errors, returning the stored records.
///
/// The records have to be real: a mined candidate carries typed `Event` sources
/// with composite foreign keys, so events that exist only in memory produce a
/// candidate the database refuses — and one that, if it were accepted, no
/// erasure entering through the session could ever reach.
async fn durable_errors(
    test_db: &moa_test_support::postgres::TestDb,
    tenant_id: TenantId,
    tool_name: &str,
    count: usize,
) -> Vec<EventRecord> {
    let session_id = moa_core::traits::SessionStore::create_session(
        test_db.store(),
        moa_core::types::session::SessionMeta {
            tenant_id,
            created_by: Some(moa_core::types::contact::SessionActorRef::Identity {
                id: Uuid::from_u128(1),
            }),
            model: moa_core::types::identifiers::ModelId::new("scripted-mining-model"),
            ..moa_core::types::session::SessionMeta::default()
        },
    )
    .await
    .expect("create the session the mined events belong to");
    let appends = (0..count)
        .map(|_| moa_session::EventAppend {
            event: Event::ToolError {
                tool_id: ToolCallId::new(),
                provider_tool_use_id: None,
                tool_name: tool_name.to_string(),
                error: "terminal failure".to_string(),
                retryable: false,
            },
            dedupe_key: None,
        })
        .collect::<Vec<_>>();
    test_db
        .store()
        .append_events(session_id, appends)
        .await
        .expect("persist the durable tool errors mining reads")
}

async fn open_candidates(
    test_db: &moa_test_support::postgres::TestDb,
    tenant_id: TenantId,
) -> Vec<moa_core::types::experience::LearningCandidate> {
    test_db
        .store()
        .list_learning_candidates(
            &tenant_id.to_string(),
            Some(LearningCandidateStatus::NeedsAuthoring),
            50,
        )
        .await
        .expect("list open candidates")
}

//! Integration tests for reviewable skill draft proposals.

#![recursion_limit = "256"]

#[path = "support/draft_proposals.rs"]
mod support;

use moa_artifacts::document::{ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::ArtifactRegistry;
use moa_core::{LearningCandidateStatus, LearningCandidateType};
use moa_skills::distiller::{DistillationOutcome, distill_skill_with_learning};
use moa_skills::improver::{ImprovementResult, improve_skill_with_learning};
use support::{
    BASELINE_SKILL, IMPROVED_SKILL, SESSION_WITH_5_TOOL_CALLS, active_semantic_version,
    artifact_revision_count, learning_store, load_optional_active_skill, load_session_fixture,
    scripted_router, seed_skill, session_storage_partition_id, setup_test_db, skill_markdown,
    skill_row_count, tenant_scope, test_config,
};

#[tokio::test]
async fn skill_creation_proposal_stores_draft_artifact_without_active_skill_db() {
    // Pins: generated skill creation remains a draft artifact and review candidate until accepted.
    let test_db = setup_test_db().await;
    let loaded = load_session_fixture(SESSION_WITH_5_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let proposed = skill_markdown(
        "draft-oauth-refresh",
        "Capture the OAuth refresh workflow as a draft",
        "Use the same repro, search, patch, and validation flow.",
        "1.0",
    );
    let store = learning_store(&test_db);

    let outcome = distill_skill_with_learning(
        &config,
        &loaded.session,
        &loaded.events,
        scripted_router([proposed]),
        Some(store.clone()),
    )
    .await
    .expect("distill draft skill");

    let DistillationOutcome::NewSkillProposed { proposal } = outcome else {
        panic!("expected creation proposal");
    };
    assert_eq!(proposal.metadata.name, "draft-oauth-refresh");
    let storage_partition_id = session_storage_partition_id(&loaded.session);
    let scope = tenant_scope(&storage_partition_id);
    assert!(
        load_optional_active_skill(&test_db, &scope, "draft-oauth-refresh")
            .await
            .is_none(),
        "draft proposal must not create an active skill row"
    );

    let candidate = store
        .get_learning_candidate(&loaded.session.tenant_id, proposal.candidate_id)
        .await
        .expect("load proposed candidate")
        .expect("candidate exists");
    assert_eq!(candidate.status, LearningCandidateStatus::Proposed);
    assert_eq!(candidate.candidate_type, LearningCandidateType::Skill);
    assert_eq!(candidate.payload["kind"], "skill_draft_proposal");
    assert_eq!(candidate.payload["operation"], "skill_created");
    assert_eq!(
        candidate.payload["draft_artifact_revision_uid"],
        proposal.draft_artifact_revision_uid.to_string()
    );
    assert_eq!(
        candidate.payload["generated_regression_suite"]["source_format"],
        "toml"
    );
    assert!(
        candidate.payload["generated_regression_suite"]["source_text"]
            .as_str()
            .expect("suite source is string")
            .contains("[[cases]]")
    );
    assert_eq!(
        artifact_revision_count(&test_db, &storage_partition_id, "draft-oauth-refresh").await,
        1
    );

    let revision = ArtifactRegistry::new(test_db.store().pool().clone())
        .load_revision(&scope, proposal.draft_artifact_revision_uid)
        .await
        .expect("load draft artifact")
        .expect("draft artifact revision exists");
    assert_eq!(revision.kind, ArtifactKind::Skill);
    assert_eq!(revision.status, ArtifactStatus::Draft);
}

#[tokio::test]
async fn skill_improvement_proposal_stores_draft_artifact_without_replacing_active_skill_db() {
    // Pins: generated skill improvement stores a draft and leaves the active skill unchanged.
    let test_db = setup_test_db().await;
    let loaded = load_session_fixture(SESSION_WITH_5_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let storage_partition_id = session_storage_partition_id(&loaded.session);
    let scope = tenant_scope(&storage_partition_id);
    let existing = seed_skill(&test_db, scope, BASELINE_SKILL).await;
    let store = learning_store(&test_db);

    let result = improve_skill_with_learning(
        &config,
        &loaded.session,
        &existing,
        &loaded.events,
        scripted_router([IMPROVED_SKILL]),
        Some(store.clone()),
    )
    .await
    .expect("propose improved skill");

    let ImprovementResult::Improved {
        proposal,
        previous_version,
        version,
    } = result
    else {
        panic!("expected improvement proposal");
    };
    assert_eq!(previous_version, "1.2");
    assert_eq!(version, "1.3");
    assert_eq!(
        active_semantic_version(&test_db, &scope, "auth-flow").await,
        "1.2"
    );
    assert_eq!(
        skill_row_count(&test_db, &storage_partition_id, "auth-flow").await,
        1
    );

    let candidate = store
        .get_learning_candidate(&loaded.session.tenant_id, proposal.candidate_id)
        .await
        .expect("load improvement candidate")
        .expect("candidate exists");
    assert_eq!(candidate.status, LearningCandidateStatus::Proposed);
    assert_eq!(candidate.payload["operation"], "skill_improved");
    assert_eq!(candidate.payload["previous_version"], "1.2");
    assert_eq!(
        candidate.payload["draft_artifact_revision_uid"],
        proposal.draft_artifact_revision_uid.to_string()
    );
    assert_eq!(
        artifact_revision_count(&test_db, &storage_partition_id, "auth-flow").await,
        2,
        "seeded published artifact plus one draft improvement revision"
    );
}

#[tokio::test]
async fn skill_proposal_retry_reuses_candidate_id() {
    // Pins: retrying the same proposal reuses the candidate and draft artifact revision.
    let test_db = setup_test_db().await;
    let loaded = load_session_fixture(SESSION_WITH_5_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let proposed = skill_markdown(
        "retry-stable-draft",
        "Keep proposal retries idempotent",
        "Use the same generated draft when workflow delivery retries.",
        "1.0",
    );
    let store = learning_store(&test_db);

    let first = distill_skill_with_learning(
        &config,
        &loaded.session,
        &loaded.events,
        scripted_router([proposed.clone()]),
        Some(store.clone()),
    )
    .await
    .expect("first proposal");
    let second = distill_skill_with_learning(
        &config,
        &loaded.session,
        &loaded.events,
        scripted_router([proposed]),
        Some(store.clone()),
    )
    .await
    .expect("retry proposal");

    let DistillationOutcome::NewSkillProposed { proposal: first } = first else {
        panic!("expected first proposal");
    };
    let DistillationOutcome::NewSkillProposed { proposal: second } = second else {
        panic!("expected second proposal");
    };
    let storage_partition_id = session_storage_partition_id(&loaded.session);
    assert_eq!(first.candidate_id, second.candidate_id);
    assert_eq!(
        first.draft_artifact_revision_uid,
        second.draft_artifact_revision_uid
    );
    assert_eq!(
        artifact_revision_count(&test_db, &storage_partition_id, "retry-stable-draft").await,
        1
    );
}

#[tokio::test]
async fn concurrent_skill_proposal_attempts_share_one_draft_artifact_db() {
    // Pins: duplicate workers proposing the same skill share one candidate and one draft revision.
    let test_db = setup_test_db().await;
    let loaded = load_session_fixture(SESSION_WITH_5_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let proposed = skill_markdown(
        "concurrent-stable-draft",
        "Keep concurrent draft proposals idempotent",
        "Use one durable candidate boundary when multiple workers retry.",
        "1.0",
    );
    let store = learning_store(&test_db);

    let (first, second) = tokio::join!(
        distill_skill_with_learning(
            &config,
            &loaded.session,
            &loaded.events,
            scripted_router([proposed.clone()]),
            Some(store.clone()),
        ),
        distill_skill_with_learning(
            &config,
            &loaded.session,
            &loaded.events,
            scripted_router([proposed]),
            Some(store.clone()),
        )
    );

    let DistillationOutcome::NewSkillProposed { proposal: first } =
        first.expect("first concurrent proposal")
    else {
        panic!("expected first proposal");
    };
    let DistillationOutcome::NewSkillProposed { proposal: second } =
        second.expect("second concurrent proposal")
    else {
        panic!("expected second proposal");
    };
    let storage_partition_id = session_storage_partition_id(&loaded.session);
    assert_eq!(first.candidate_id, second.candidate_id);
    assert_eq!(
        first.draft_artifact_revision_uid,
        second.draft_artifact_revision_uid
    );
    assert_eq!(
        artifact_revision_count(&test_db, &storage_partition_id, "concurrent-stable-draft").await,
        1
    );
}

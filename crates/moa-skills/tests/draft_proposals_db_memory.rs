//! Integration tests for reviewable skill draft proposals.

#![recursion_limit = "256"]

#[path = "support/common.rs"]
mod support;

use moa_artifacts::document::{ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::ArtifactRegistry;
use moa_core::{
    types::experience::LearningCandidateStatus, types::experience::LearningCandidateType,
};
use std::sync::atomic::Ordering;

use moa_skills::distiller::{
    DispatchEvidence, DistillationOutcome, distill_skill_from_experience_with_learning,
};
use moa_skills::improver::{ImprovementResult, improve_skill_from_experience_with_learning};
use moa_skills::proposals::{
    RecurrenceSiblingSuite, SiblingResynthesis, accumulate_recurrence_siblings,
};
use support::{
    BASELINE_SKILL, IMPROVED_SKILL, SESSION_WITH_8_TOOL_CALLS, active_semantic_version,
    artifact_revision_count, experience_input, learning_store, load_optional_active_skill,
    load_session_fixture, race_mutating_router, scripted_router, seed_skill,
    session_storage_partition_id, setup_test_db, skill_markdown, skill_row_count, tenant_scope,
    test_config,
};

#[tokio::test]
async fn skill_creation_proposal_stores_draft_artifact_without_active_skill_db() {
    // Pins: generated skill creation remains a draft artifact and review candidate until accepted.
    let test_db = setup_test_db().await;
    let loaded = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let proposed = skill_markdown(
        "draft-oauth-refresh",
        "Capture the OAuth refresh workflow as a draft",
        "Use the same repro, search, patch, and validation flow.",
        "1.0",
    );
    let store = learning_store(&test_db);

    let outcome = distill_skill_from_experience_with_learning(
        &config,
        &loaded.session,
        experience_input(&loaded, "capture the oauth refresh workflow"),
        scripted_router([proposed]),
        Some(store.clone()),
        None,
        &DispatchEvidence::SingleSession,
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
    let evidence = &candidate.payload["evidence"];
    assert_eq!(evidence["outcome"], "resolved");
    assert_eq!(evidence["confidence"], 0.9);
    assert_eq!(
        evidence["task_summary"], "capture the oauth refresh workflow",
        "reviewers see the assessed task behind the proposal"
    );
    assert_eq!(evidence["routing"]["decision"], "create_new");
    assert_eq!(
        evidence["segment_evidence"][0]["summary"], "verification tool run passed",
        "segment-assessment evidence rows are surfaced for review"
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
    let files = ArtifactRegistry::new(test_db.store().pool().clone())
        .load_files(&scope, proposal.draft_artifact_revision_uid)
        .await
        .expect("load draft files");
    assert!(
        files
            .iter()
            .any(|file| file.path == "tests/regression-suite.toml"),
        "the generated suite must ride the draft package as held-out material for later revisions"
    );
}

#[tokio::test]
async fn skill_improvement_proposal_stores_draft_artifact_without_replacing_active_skill_db() {
    // Pins: generated skill improvement stores a draft and leaves the active skill unchanged.
    let test_db = setup_test_db().await;
    let loaded = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    let (_config, _temp_dir) = test_config(&test_db);
    let storage_partition_id = session_storage_partition_id(&loaded.session);
    let scope = tenant_scope(&storage_partition_id);
    let existing = seed_skill(&test_db, scope, BASELINE_SKILL).await;
    let store = learning_store(&test_db);
    let improvement_input = experience_input(&loaded, "improve the auth flow skill");

    let result = improve_skill_from_experience_with_learning(
        &loaded.session,
        &existing,
        &improvement_input,
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
    let loaded = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let proposed = skill_markdown(
        "retry-stable-draft",
        "Keep proposal retries idempotent",
        "Use the same generated draft when workflow delivery retries.",
        "1.0",
    );
    let store = learning_store(&test_db);

    let retried_input = experience_input(&loaded, "capture the oauth refresh workflow");

    let first = distill_skill_from_experience_with_learning(
        &config,
        &loaded.session,
        retried_input.clone(),
        scripted_router([proposed.clone()]),
        Some(store.clone()),
        None,
        &DispatchEvidence::SingleSession,
    )
    .await
    .expect("first proposal");
    // The retry passes an empty scripted router: the preflight dedupe must return
    // the open proposal before any LLM call, so an attempted call would error.
    let second = distill_skill_from_experience_with_learning(
        &config,
        &loaded.session,
        retried_input,
        scripted_router(Vec::<String>::new()),
        Some(store.clone()),
        None,
        &DispatchEvidence::SingleSession,
    )
    .await
    .expect("retry proposal");

    let DistillationOutcome::NewSkillProposed { proposal: first } = first else {
        panic!("expected first proposal");
    };
    // A replay of the proposal's own experience dedupes onto the open candidate and
    // accepts no new sibling, so nothing is re-synthesized.
    let DistillationOutcome::DedupedOntoOpenProposal {
        proposal: second,
        resynthesis,
    } = second
    else {
        panic!("expected retry to dedupe onto the open proposal");
    };
    assert_eq!(resynthesis, SiblingResynthesis::DraftUnchanged);
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
    let candidate = store
        .get_learning_candidate(&loaded.session.tenant_id, first.candidate_id)
        .await
        .expect("reload retried candidate")
        .expect("candidate exists");
    assert!(
        candidate
            .payload
            .get("accumulated_regression_suites")
            .is_none(),
        "a replay of the proposal's own experience is not a sibling suite"
    );
}

#[tokio::test]
async fn open_proposal_for_same_skill_name_dedupes_across_sessions_db() {
    // Pins: a second qualifying experience from a different session that generates the same
    // skill name reuses the open Proposed candidate instead of filing a duplicate review
    // item and a second draft artifact revision.
    let test_db = setup_test_db().await;
    let loaded_a = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    let mut loaded_b = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    loaded_b.session.id = moa_core::types::identifiers::SessionId::new();
    loaded_b.session.tenant_id = loaded_a.session.tenant_id;
    let (config, _temp_dir) = test_config(&test_db);
    let proposed = skill_markdown(
        "dedup-stable-draft",
        "Keep the review queue to one open proposal per skill",
        "Reuse the open candidate when the same skill recurs.",
        "1.0",
    );
    let store = learning_store(&test_db);

    let first = distill_skill_from_experience_with_learning(
        &config,
        &loaded_a.session,
        experience_input(&loaded_a, "sync tickets"),
        scripted_router([proposed.clone()]),
        Some(store.clone()),
        None,
        &DispatchEvidence::SingleSession,
    )
    .await
    .expect("first experience proposal");
    let second = distill_skill_from_experience_with_learning(
        &config,
        &loaded_b.session,
        experience_input(&loaded_b, "sync tickets again"),
        scripted_router([proposed]),
        Some(store.clone()),
        None,
        &DispatchEvidence::SingleSession,
    )
    .await
    .expect("second experience proposal");

    let DistillationOutcome::NewSkillProposed { proposal: first } = first else {
        panic!("expected first proposal");
    };
    let DistillationOutcome::NewSkillProposed { proposal: second } = second else {
        panic!("expected second proposal");
    };
    let storage_partition_id = session_storage_partition_id(&loaded_a.session);
    assert_eq!(
        first.candidate_id, second.candidate_id,
        "second proposal must reuse the open candidate"
    );
    assert_eq!(
        first.draft_artifact_revision_uid,
        second.draft_artifact_revision_uid
    );
    assert_eq!(
        artifact_revision_count(&test_db, &storage_partition_id, "dedup-stable-draft").await,
        1,
        "duplicate proposal must not create a second draft revision"
    );
}

#[tokio::test]
async fn open_proposal_for_same_task_fingerprint_dedupes_across_skill_names_db() {
    // Pins: when the LLM names the same recurring task differently across sessions, the
    // open Proposed candidate for that task fingerprint is reused instead of filing a
    // second near-duplicate review item under the new name.
    let test_db = setup_test_db().await;
    let loaded_a = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    let mut loaded_b = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    loaded_b.session.id = moa_core::types::identifiers::SessionId::new();
    loaded_b.session.tenant_id = loaded_a.session.tenant_id;
    let (config, _temp_dir) = test_config(&test_db);
    let first_name = skill_markdown(
        "deploy-to-staging",
        "Deploy the service to staging",
        "Reusable deploy workflow.",
        "1.0",
    );
    let second_name = skill_markdown(
        "staging-deployment",
        "Deploy the service to staging",
        "Reusable deploy workflow.",
        "1.0",
    );
    let store = learning_store(&test_db);

    // Same task summary => same fixture fingerprint hash, different generated names.
    let first = distill_skill_from_experience_with_learning(
        &config,
        &loaded_a.session,
        experience_input(&loaded_a, "deploy service to staging"),
        scripted_router([first_name]),
        Some(store.clone()),
        None,
        &DispatchEvidence::SingleSession,
    )
    .await
    .expect("first fingerprint proposal");
    // Empty scripted router: the fingerprint preflight must dedupe before any
    // LLM call, so an attempted generation would error the test.
    let _unused_second_name = second_name;
    let input_b = experience_input(&loaded_b, "deploy service to staging");
    let sibling_experience_id = input_b.experience.id;
    let second = distill_skill_from_experience_with_learning(
        &config,
        &loaded_b.session,
        input_b,
        scripted_router(Vec::<String>::new()),
        Some(store.clone()),
        None,
        &DispatchEvidence::SingleSession,
    )
    .await
    .expect("second fingerprint proposal");

    let DistillationOutcome::NewSkillProposed { proposal: first } = first else {
        panic!("expected first proposal");
    };
    // The differently-named sibling dedupes on the shared fingerprint. Its empty
    // scripted router makes the generalization pass error out, which is swallowed,
    // so the draft is left unchanged.
    let DistillationOutcome::DedupedOntoOpenProposal {
        proposal: second,
        resynthesis,
    } = second
    else {
        panic!("expected second proposal to dedupe onto the open candidate");
    };
    assert_eq!(resynthesis, SiblingResynthesis::DraftUnchanged);
    let storage_partition_id = session_storage_partition_id(&loaded_a.session);
    assert_eq!(
        first.candidate_id, second.candidate_id,
        "same-fingerprint proposal must reuse the open candidate despite the new name"
    );
    assert_eq!(
        artifact_revision_count(&test_db, &storage_partition_id, "deploy-to-staging").await,
        1
    );
    assert_eq!(
        artifact_revision_count(&test_db, &storage_partition_id, "staging-deployment").await,
        0,
        "the differently-named duplicate must not create its own draft artifact"
    );
    let candidate = store
        .get_learning_candidate(&loaded_a.session.tenant_id, first.candidate_id)
        .await
        .expect("reload deduped candidate")
        .expect("candidate exists");
    let siblings = candidate.payload["accumulated_regression_suites"]
        .as_array()
        .expect("deduped session accumulates a sibling suite");
    assert_eq!(siblings.len(), 1);
    assert_eq!(
        siblings[0]["source_experience_id"],
        sibling_experience_id.to_string(),
        "sibling suite records which experience contributed it"
    );
    assert!(
        siblings[0]["source_text"]
            .as_str()
            .expect("sibling suite carries TOML")
            .contains("[[cases]]")
    );
}

#[tokio::test]
async fn concurrent_skill_proposal_attempts_share_one_draft_artifact_db() {
    // Pins: duplicate workers proposing the same skill share one candidate and one draft revision.
    let test_db = setup_test_db().await;
    let loaded = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let proposed = skill_markdown(
        "concurrent-stable-draft",
        "Keep concurrent draft proposals idempotent",
        "Use one durable candidate boundary when multiple workers retry.",
        "1.0",
    );
    let store = learning_store(&test_db);
    let shared_input = experience_input(&loaded, "capture the oauth refresh workflow");

    let (first, second) = tokio::join!(
        distill_skill_from_experience_with_learning(
            &config,
            &loaded.session,
            shared_input.clone(),
            scripted_router([proposed.clone()]),
            Some(store.clone()),
            None,
            &DispatchEvidence::SingleSession,
        ),
        distill_skill_from_experience_with_learning(
            &config,
            &loaded.session,
            shared_input.clone(),
            scripted_router([proposed]),
            Some(store.clone()),
            None,
            &DispatchEvidence::SingleSession,
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

/// Runs one sibling experience against an open proposal for the same task, returning the deduped
/// candidate id and whether the generalization pass rewrote the open draft. Each sibling gets a
/// fresh session under the origin tenant so the fingerprint dedupe lands on the open candidate.
async fn distill_sibling(
    config: &moa_core::config::MoaConfig,
    store: &std::sync::Arc<moa_session::PostgresSessionStore>,
    origin: &support::LoadedSession,
    task_summary: &str,
    generalization: impl Into<String>,
) -> (uuid::Uuid, SiblingResynthesis) {
    let mut sibling = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    sibling.session.id = moa_core::types::identifiers::SessionId::new();
    sibling.session.tenant_id = origin.session.tenant_id;
    let outcome = distill_skill_from_experience_with_learning(
        config,
        &sibling.session,
        experience_input(&sibling, task_summary),
        scripted_router([generalization.into()]),
        Some(store.clone()),
        None,
        &DispatchEvidence::SingleSession,
    )
    .await
    .expect("sibling proposal");
    let DistillationOutcome::DedupedOntoOpenProposal {
        proposal,
        resynthesis,
    } = outcome
    else {
        panic!("sibling must dedupe onto the open creation candidate");
    };
    (proposal.candidate_id, resynthesis)
}

#[tokio::test]
async fn sibling_experience_resynthesizes_the_open_draft_db() {
    // Pins: a second qualifying experience for the same task dedupes onto the open Proposed
    // candidate and runs exactly one generalization pass that rewrites the draft skill_markdown,
    // stores a new draft revision, and records a resynthesis evidence entry with a stability score.
    let test_db = setup_test_db().await;
    let origin = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let created = skill_markdown(
        "resynth-flow",
        "Capture the recurring workflow",
        "Original single-instance body.",
        "1.0",
    );
    let generalized = skill_markdown(
        "resynth-flow",
        "Capture the recurring workflow",
        "Parameterized body with an explicit {ticket} slot and invariant steps.",
        "1.0",
    );
    let store = learning_store(&test_db);

    let first = distill_skill_from_experience_with_learning(
        &config,
        &origin.session,
        experience_input(&origin, "sync tickets"),
        scripted_router([created]),
        Some(store.clone()),
        None,
        &DispatchEvidence::SingleSession,
    )
    .await
    .expect("first proposal");
    let DistillationOutcome::NewSkillProposed { proposal: first } = first else {
        panic!("expected creation proposal");
    };

    let (sibling_candidate, resynthesis) =
        distill_sibling(&config, &store, &origin, "sync tickets", generalized).await;
    assert_eq!(
        sibling_candidate, first.candidate_id,
        "sibling must reuse the open candidate"
    );
    assert_eq!(
        resynthesis,
        SiblingResynthesis::DraftRewritten,
        "a changed generalization pass reports a re-synthesized draft"
    );

    let candidate = store
        .get_learning_candidate(&origin.session.tenant_id, first.candidate_id)
        .await
        .expect("load candidate")
        .expect("candidate exists");
    assert!(
        candidate.payload["skill_markdown"]
            .as_str()
            .expect("draft markdown")
            .contains("Parameterized body"),
        "the draft skill_markdown must be re-synthesized to the generalized body"
    );
    assert_ne!(
        candidate.payload["draft_artifact_revision_uid"]
            .as_str()
            .expect("draft revision uid"),
        first.draft_artifact_revision_uid.to_string(),
        "re-synthesis must store a new draft revision"
    );
    let passes = candidate.payload["resynthesis"]
        .as_array()
        .expect("resynthesis evidence array");
    assert_eq!(passes.len(), 1, "exactly one generalization pass ran");
    assert_eq!(passes[0]["changed"], true);
    assert!(
        passes[0]["trajectory_stability"].is_number(),
        "each pass records a trajectory stability score"
    );

    let storage_partition_id = session_storage_partition_id(&origin.session);
    assert_eq!(
        artifact_revision_count(&test_db, &storage_partition_id, "resynth-flow").await,
        2,
        "original draft plus the re-synthesized draft revision"
    );
}

#[tokio::test]
async fn sibling_resynthesis_unchanged_keeps_draft_but_records_evidence_db() {
    // Pins: when the generalization model returns UNCHANGED, the open draft revision and its
    // markdown are left as-is, but the pass is still recorded (changed=false) with its stability
    // score so the review gate sees the recurrence was considered.
    let test_db = setup_test_db().await;
    let origin = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let created = skill_markdown(
        "resynth-stable",
        "Capture the recurring workflow",
        "Original single-instance body.",
        "1.0",
    );
    let store = learning_store(&test_db);

    let first = distill_skill_from_experience_with_learning(
        &config,
        &origin.session,
        experience_input(&origin, "sync tickets"),
        scripted_router([created]),
        Some(store.clone()),
        None,
        &DispatchEvidence::SingleSession,
    )
    .await
    .expect("first proposal");
    let DistillationOutcome::NewSkillProposed { proposal: first } = first else {
        panic!("expected creation proposal");
    };

    let (_, resynthesis) =
        distill_sibling(&config, &store, &origin, "sync tickets", "UNCHANGED").await;
    assert_eq!(
        resynthesis,
        SiblingResynthesis::DraftUnchanged,
        "an UNCHANGED generalization pass filed no re-synthesized draft"
    );

    let candidate = store
        .get_learning_candidate(&origin.session.tenant_id, first.candidate_id)
        .await
        .expect("load candidate")
        .expect("candidate exists");
    assert!(
        candidate.payload["skill_markdown"]
            .as_str()
            .expect("draft markdown")
            .contains("Original single-instance body"),
        "an UNCHANGED pass must not rewrite the draft markdown"
    );
    assert_eq!(
        candidate.payload["draft_artifact_revision_uid"],
        first.draft_artifact_revision_uid.to_string(),
        "an UNCHANGED pass must not store a new draft revision"
    );
    let passes = candidate.payload["resynthesis"]
        .as_array()
        .expect("resynthesis evidence array");
    assert_eq!(passes.len(), 1);
    assert_eq!(passes[0]["changed"], false);
    assert!(passes[0]["trajectory_stability"].is_number());

    let storage_partition_id = session_storage_partition_id(&origin.session);
    assert_eq!(
        artifact_revision_count(&test_db, &storage_partition_id, "resynth-stable").await,
        1,
        "UNCHANGED leaves the single draft revision in place"
    );
}

#[tokio::test]
async fn sibling_resynthesis_stops_at_the_pass_cap_db() {
    // Pins: generalization passes are capped like sibling-suite accumulation. After the cap is
    // reached a further sibling accumulates nothing and spends no model call (its empty scripted
    // router is never touched), leaving the recorded pass count pinned at the cap.
    let test_db = setup_test_db().await;
    let origin = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let created = skill_markdown(
        "resynth-capped",
        "Capture the recurring workflow",
        "Original single-instance body.",
        "1.0",
    );
    let generalized = skill_markdown(
        "resynth-capped",
        "Capture the recurring workflow",
        "Parameterized body with an explicit {ticket} slot and invariant steps.",
        "1.0",
    );
    let store = learning_store(&test_db);

    let first = distill_skill_from_experience_with_learning(
        &config,
        &origin.session,
        experience_input(&origin, "sync tickets"),
        scripted_router([created]),
        Some(store.clone()),
        None,
        &DispatchEvidence::SingleSession,
    )
    .await
    .expect("first proposal");
    let DistillationOutcome::NewSkillProposed { proposal: first } = first else {
        panic!("expected creation proposal");
    };

    // Three accepted siblings fill both the sibling-suite and resynthesis caps.
    for _ in 0..3 {
        let (_, resynthesis) = distill_sibling(
            &config,
            &store,
            &origin,
            "sync tickets",
            generalized.clone(),
        )
        .await;
        assert_eq!(resynthesis, SiblingResynthesis::DraftRewritten);
    }
    let candidate = store
        .get_learning_candidate(&origin.session.tenant_id, first.candidate_id)
        .await
        .expect("load candidate")
        .expect("candidate exists");
    assert_eq!(
        candidate.payload["resynthesis"]
            .as_array()
            .expect("resynthesis evidence array")
            .len(),
        3,
        "resynthesis passes are capped at the sibling-suite maximum"
    );

    // A fourth sibling passes an empty scripted router: if the capped candidate spent a model
    // call it would draw from the exhausted provider. Accumulation short-circuits at the cap, so
    // no call is made and the pass count stays pinned.
    let mut over_cap = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    over_cap.session.id = moa_core::types::identifiers::SessionId::new();
    over_cap.session.tenant_id = origin.session.tenant_id;
    distill_skill_from_experience_with_learning(
        &config,
        &over_cap.session,
        experience_input(&over_cap, "sync tickets"),
        scripted_router(Vec::<String>::new()),
        Some(store.clone()),
        None,
        &DispatchEvidence::SingleSession,
    )
    .await
    .expect("over-cap sibling proposal");

    let candidate = store
        .get_learning_candidate(&origin.session.tenant_id, first.candidate_id)
        .await
        .expect("reload candidate")
        .expect("candidate exists");
    assert_eq!(
        candidate.payload["resynthesis"]
            .as_array()
            .expect("resynthesis evidence array")
            .len(),
        3,
        "a sibling past the cap must not add another generalization pass"
    );
}

#[tokio::test]
async fn recurrence_siblings_generalize_in_one_combined_pass_db() {
    // Pins Simplification A: the recurrence path accumulates every sibling's suite,
    // then runs EXACTLY ONE generalization model call covering all of them, rather
    // than one paid call per sibling. The router is primed with a single response,
    // so a per-sibling loop would need a second call and fail — the run only
    // succeeds because there is a single combined pass.
    let test_db = setup_test_db().await;
    let origin = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let store = learning_store(&test_db);
    let created = skill_markdown(
        "recurrence-combined",
        "Capture recurring work",
        "Original single-instance body.",
        "1.0",
    );
    let generalized = skill_markdown(
        "recurrence-combined",
        "Capture recurring work",
        "Parameterized body with an explicit {slot} for every instance.",
        "1.0",
    );

    let first = distill_skill_from_experience_with_learning(
        &config,
        &origin.session,
        experience_input(&origin, "combine siblings"),
        scripted_router([created]),
        Some(store.clone()),
        None,
        &DispatchEvidence::SingleSession,
    )
    .await
    .expect("first proposal");
    let DistillationOutcome::NewSkillProposed { proposal: open } = first else {
        panic!("expected creation proposal");
    };

    let sibling_a = uuid::Uuid::now_v7();
    let sibling_b = uuid::Uuid::now_v7();
    let siblings = vec![
        RecurrenceSiblingSuite {
            events: &origin.events,
            source_experience_id: sibling_a,
            source_session_id: origin.session.id,
        },
        RecurrenceSiblingSuite {
            events: &origin.events,
            source_experience_id: sibling_b,
            source_session_id: origin.session.id,
        },
    ];
    // Exactly one scripted response: a second model call would exhaust it and error.
    let router = scripted_router([generalized]);
    let resynthesis =
        accumulate_recurrence_siblings(&store, &router, origin.session.tenant_id, &open, &siblings)
            .await
            .expect("combined accumulation");
    assert_eq!(
        resynthesis,
        SiblingResynthesis::DraftRewritten,
        "the single combined pass rewrote the draft"
    );

    let candidate = store
        .get_learning_candidate(&origin.session.tenant_id, open.candidate_id)
        .await
        .expect("load candidate")
        .expect("candidate exists");
    assert_eq!(
        candidate.payload["accumulated_regression_suites"]
            .as_array()
            .expect("sibling suites")
            .len(),
        2,
        "both siblings' suites pool as held-out material"
    );
    let passes = candidate.payload["resynthesis"]
        .as_array()
        .expect("resynthesis evidence array");
    assert_eq!(
        passes.len(),
        1,
        "N siblings collapse into a single combined generalization pass"
    );
    let recorded: std::collections::HashSet<&str> = passes[0]["source_experience_ids"]
        .as_array()
        .expect("combined source ids")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert!(
        recorded.contains(sibling_a.to_string().as_str())
            && recorded.contains(sibling_b.to_string().as_str()),
        "the combined pass records every contributing sibling"
    );
    assert!(
        candidate.payload["skill_markdown"]
            .as_str()
            .expect("draft markdown")
            .contains("Parameterized body"),
        "the draft is generalized once for both siblings"
    );
}

#[tokio::test]
async fn concurrent_resynthesis_retries_instead_of_clobbering_db() {
    // Pins F8: when the draft revision changes under a generalization pass (a rival
    // pass landed while the model call was in flight), the under-lock write detects
    // the changed revision, retries against the latest draft, and does not clobber
    // the rival. The mutating provider advances the draft revision on its first call
    // only, so the first apply conflicts and the retry lands: two model calls, one
    // recorded pass. Without the optimistic check the first apply would clobber the
    // rival and only one model call would run.
    let test_db = setup_test_db().await;
    let origin = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let store = learning_store(&test_db);
    let created = skill_markdown(
        "resynth-race",
        "Capture recurring work",
        "Original single-instance body.",
        "1.0",
    );
    let generalized = skill_markdown(
        "resynth-race",
        "Capture recurring work",
        "Parameterized body with an explicit {slot}.",
        "1.0",
    );

    let first = distill_skill_from_experience_with_learning(
        &config,
        &origin.session,
        experience_input(&origin, "race the draft"),
        scripted_router([created]),
        Some(store.clone()),
        None,
        &DispatchEvidence::SingleSession,
    )
    .await
    .expect("first proposal");
    let DistillationOutcome::NewSkillProposed { proposal: open } = first else {
        panic!("expected creation proposal");
    };

    let (router, calls) = race_mutating_router(
        store.clone(),
        origin.session.tenant_id,
        open.candidate_id,
        generalized,
    );
    let sibling = uuid::Uuid::now_v7();
    let siblings = vec![RecurrenceSiblingSuite {
        events: &origin.events,
        source_experience_id: sibling,
        source_session_id: origin.session.id,
    }];
    let resynthesis =
        accumulate_recurrence_siblings(&store, &router, origin.session.tenant_id, &open, &siblings)
            .await
            .expect("accumulation");

    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the pass detected the concurrent rewrite and retried once"
    );
    assert_eq!(
        resynthesis,
        SiblingResynthesis::DraftRewritten,
        "the retry generalized the rival's draft"
    );
    let candidate = store
        .get_learning_candidate(&origin.session.tenant_id, open.candidate_id)
        .await
        .expect("load candidate")
        .expect("candidate exists");
    assert_eq!(
        candidate.payload["resynthesis"]
            .as_array()
            .expect("resynthesis evidence array")
            .len(),
        1,
        "the conflicting attempt recorded no pass; only the applied retry did"
    );
    assert!(
        candidate.payload["skill_markdown"]
            .as_str()
            .expect("draft markdown")
            .contains("Parameterized body"),
        "the applied retry generalized the draft"
    );
}

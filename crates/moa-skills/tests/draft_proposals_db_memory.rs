//! Integration tests for reviewable skill draft proposals.

#![recursion_limit = "256"]

#[path = "support/common.rs"]
mod support;

use moa_artifacts::document::{ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, SuiteContributionKind};
use moa_core::{
    types::experience::LearningCandidateStatus, types::experience::LearningCandidateType,
};
use moa_skills::distiller::{
    DispatchEvidence, DistillationOutcome, distill_skill_from_experience_with_learning,
};
use moa_skills::improver::{ImprovementResult, improve_skill_from_experience_with_learning};
use moa_skills::proposals::{RecurrenceSiblingSuite, accumulate_recurrence_siblings};
use support::{
    BASELINE_SKILL, IMPROVED_SKILL, SESSION_WITH_8_TOOL_CALLS, active_semantic_version,
    artifact_revision_count, learning_store, load_optional_active_skill, load_session_fixture,
    scripted_router, seed_skill, seeded_experience_input, session_storage_partition_id,
    setup_test_db, skill_markdown, skill_row_count, tenant_scope, test_config,
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
        seeded_experience_input(&test_db, &loaded, "capture the oauth refresh workflow").await,
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
    // The generated suite is attributable text, so it lives in an owned row that
    // names its source session and experience — not in candidate JSON, where no
    // erasure could join to it or delete it for one subject.
    assert!(
        candidate
            .payload
            .get("generated_regression_suite")
            .is_none(),
        "suite bytes must not survive in the candidate payload"
    );
    let registry = ArtifactRegistry::new(test_db.store().pool().clone());
    let contributions = registry
        .list_suite_contributions(&scope, candidate.id)
        .await
        .expect("load suite contributions");
    let generated = contributions
        .iter()
        .find(|contribution| contribution.kind == SuiteContributionKind::Generated)
        .expect("the proposal's own generated suite is stored");
    assert!(generated.suite_source.contains("[[cases]]"));
    assert_eq!(
        generated.source_session_id,
        Some(loaded.session.id.0),
        "the suite names the session whose transcript produced it"
    );
    assert!(
        generated.source_experience_id.is_some(),
        "the suite names the experience whose transcript produced it"
    );
    // Pins: suite reads apply the caller's tenant scope even when the candidate
    // UUID is known, so a cross-tenant lookup cannot recover attributable bytes.
    let other_scope = moa_core::types::action_policy::ActionRuleScope::Tenant {
        tenant_id: moa_core::types::identifiers::TenantId::new(),
    };
    assert!(
        registry
            .list_suite_contributions(&other_scope, candidate.id)
            .await
            .expect("cross-tenant suite lookup is a valid empty read")
            .is_empty()
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

    // Every attributable byte of the revision names the candidate that produced
    // it: the model-written definition, and each package file separately.
    //
    // The definition row is what a privacy erasure walks to decide whether a
    // serving revision must be deleted or invalidated — `enumerate_learning_closure`
    // derives both `revision_uids` and `sole_source_revision_uids` from this table
    // and nothing else. Without these rows an erasure enumerates zero revisions
    // forever: it never deletes a sole-source revision, never invalidates a shared
    // one, and every count stays truthfully zero while the skill keeps serving.
    let contributions: Vec<(String, Option<uuid::Uuid>)> = sqlx::query_as(
        "SELECT contribution_kind, file_uid FROM moa.artifact_revision_contribution \
         WHERE revision_uid = $1 AND candidate_id = $2",
    )
    .bind(proposal.draft_artifact_revision_uid)
    .bind(candidate.id)
    .fetch_all(test_db.store().pool())
    .await
    .expect("load revision contributions");
    assert_eq!(
        contributions
            .iter()
            .filter(|(kind, file_uid)| kind == "generated_definition" && file_uid.is_none())
            .count(),
        1,
        "the revision's fused model output is attributed exactly once"
    );
    assert_eq!(
        contributions
            .iter()
            .filter(|(kind, file_uid)| kind == "generated_file" && file_uid.is_some())
            .count(),
        files.len(),
        "every package file is separately attributed, so a subtractable file can be erased alone"
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
    let improvement_input =
        seeded_experience_input(&test_db, &loaded, "improve the auth flow skill").await;

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

    let retried_input =
        seeded_experience_input(&test_db, &loaded, "capture the oauth refresh workflow").await;

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
    // accepts no new sibling, so nothing changes.
    let DistillationOutcome::DedupedOntoOpenProposal { proposal: second } = second else {
        panic!("expected retry to dedupe onto the open proposal");
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
    let candidate = store
        .get_learning_candidate(&loaded.session.tenant_id, first.candidate_id)
        .await
        .expect("reload retried candidate")
        .expect("candidate exists");
    let accumulated = ArtifactRegistry::new(test_db.store().pool().clone())
        .list_suite_contributions(
            &tenant_scope(&session_storage_partition_id(&loaded.session)),
            candidate.id,
        )
        .await
        .expect("load suite contributions")
        .into_iter()
        .filter(|contribution| contribution.kind == SuiteContributionKind::Accumulated)
        .count();
    assert_eq!(
        accumulated, 0,
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
        seeded_experience_input(&test_db, &loaded_a, "sync tickets").await,
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
        seeded_experience_input(&test_db, &loaded_b, "sync tickets again").await,
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
        seeded_experience_input(&test_db, &loaded_a, "deploy service to staging").await,
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
    let input_b = seeded_experience_input(&test_db, &loaded_b, "deploy service to staging").await;
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
    // The differently-named sibling dedupes on the shared fingerprint and
    // contributes only a held-out suite, so the empty router is never called.
    let DistillationOutcome::DedupedOntoOpenProposal { proposal: second } = second else {
        panic!("expected second proposal to dedupe onto the open candidate");
    };
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
    let siblings = ArtifactRegistry::new(test_db.store().pool().clone())
        .list_suite_contributions(
            &tenant_scope(&session_storage_partition_id(&loaded_a.session)),
            candidate.id,
        )
        .await
        .expect("load suite contributions")
        .into_iter()
        .filter(|contribution| contribution.kind == SuiteContributionKind::Accumulated)
        .collect::<Vec<_>>();
    assert_eq!(
        siblings.len(),
        1,
        "deduped session accumulates a sibling suite"
    );
    assert_eq!(
        siblings[0].source_experience_id,
        Some(sibling_experience_id),
        "sibling suite records which experience contributed it"
    );
    assert_eq!(
        siblings[0].source_session_id,
        Some(loaded_b.session.id.0),
        "sibling suite records the session an erasure would enter through"
    );
    assert!(siblings[0].suite_source.contains("[[cases]]"));
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
    let shared_input =
        seeded_experience_input(&test_db, &loaded, "capture the oauth refresh workflow").await;

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

#[tokio::test]
async fn accumulated_sibling_suites_stop_at_the_cap_db() {
    // Pins: sibling evidence stays held out and the accumulated suite pool is capped.
    //
    // Each accepted sibling stores a full regression suite. Unbounded, one popular
    // recurring task would pool suites without limit, and the review gate prices and
    // executes every pooled suite — so an uncapped pool grows the gate's cost estimate
    // until promotion is blocked by budget, on a candidate that did nothing wrong.
    // The bound now lives in a row count rather than a JSON array length, which is
    // exactly why it needs asserting again.
    let test_db = setup_test_db().await;
    let origin = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let store = learning_store(&test_db);
    let created = skill_markdown(
        "recurrence-capped",
        "Capture recurring work",
        "Original single-instance body.",
        "1.0",
    );

    let first = distill_skill_from_experience_with_learning(
        &config,
        &origin.session,
        seeded_experience_input(&test_db, &origin, "cap the pool").await,
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

    // Four distinct siblings, one more than the cap.
    let mut inputs = Vec::new();
    for index in 0..4 {
        inputs.push(
            seeded_experience_input(&test_db, &origin, &format!("cap the pool {index}")).await,
        );
    }
    let siblings = inputs
        .iter()
        .map(|input| RecurrenceSiblingSuite {
            evidence: &input.evidence,
            source_experience_id: input.experience.id,
            source_session_id: origin.session.id,
        })
        .collect::<Vec<_>>();
    let accepted =
        accumulate_recurrence_siblings(&store, origin.session.tenant_id, &open, &siblings)
            .await
            .expect("accumulate four siblings");
    assert_eq!(accepted, 3);

    let accumulated = ArtifactRegistry::new(test_db.store().pool().clone())
        .list_suite_contributions(
            &tenant_scope(&session_storage_partition_id(&origin.session)),
            open.candidate_id,
        )
        .await
        .expect("load suite contributions")
        .into_iter()
        .filter(|contribution| contribution.kind == SuiteContributionKind::Accumulated)
        .count();
    assert_eq!(
        accumulated, 3,
        "the fourth sibling is refused; the pool is bounded by MAX_ACCUMULATED_SIBLING_SUITES"
    );
}

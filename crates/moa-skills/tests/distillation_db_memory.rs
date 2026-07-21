//! Integration tests for experience-backed skill distillation.

#![recursion_limit = "256"]

#[path = "support/common.rs"]
mod support;

use std::sync::atomic::Ordering;

use chrono::{Duration, Utc};
use moa_config::MoaConfig;
use moa_config::RecurrenceConfig;
use moa_core::{types::identifiers::TenantId, types::segment_assessment::SegmentOutcome};
use moa_skills::distiller::{
    DispatchEvidence, DistillationOutcome, DistillationSkipReason,
    distill_skill_from_experience_with_learning,
};
use moa_skills::recurrence::{
    RecurrenceThresholds, cluster_recurrence_groups, qualify_recurrence_cluster,
};
use support::{
    SESSION_WITH_4_TOOL_CALLS, SESSION_WITH_8_TOOL_CALLS, experience_input, learning_probe_vector,
    learning_store, load_optional_active_skill, load_session_fixture, scripted_embedder,
    scripted_router, seed_embedded_experience, seed_skill, session_storage_partition_id,
    setup_test_db, skill_markdown, tenant_scope, test_config,
};

/// Returns the fixture's task text so experience summaries match the session events.
fn fixture_task(loaded: &support::LoadedSession) -> String {
    loaded
        .session
        .title
        .clone()
        .expect("session fixture carries a task title")
}

#[tokio::test]
async fn resolved_experience_with_8_tool_calls_triggers_distillation() {
    // Pins: a learnable resolved experience above the tool-call threshold produces a
    // reviewable draft proposal without creating an active skill row.
    let test_db = setup_test_db().await;
    let loaded = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let proposed = skill_markdown(
        "oauth-refresh-regression",
        "Capture the OAuth refresh regression workflow",
        "Use the same repro, search, patch, and validation flow.",
        "1.0",
    );

    let outcome = distill_skill_from_experience_with_learning(
        &config,
        &loaded.session,
        experience_input(&loaded, &fixture_task(&loaded)),
        scripted_router([proposed]),
        Some(learning_store(&test_db)),
        None,
        &DispatchEvidence::SingleSession,
    )
    .await
    .expect("distill resolved experience");

    let DistillationOutcome::NewSkillProposed { proposal } = outcome else {
        panic!("expected new skill proposal");
    };
    assert_eq!(proposal.metadata.name, "oauth-refresh-regression");
    let storage_partition_id = session_storage_partition_id(&loaded.session);
    let scope = tenant_scope(&storage_partition_id);
    assert!(
        load_optional_active_skill(&test_db, &scope, "oauth-refresh-regression")
            .await
            .is_none()
    );
}

#[tokio::test]
async fn experience_with_4_tool_calls_does_not_trigger_distillation() {
    // Pins: segments below the configured tool-call threshold skip distillation.
    let loaded = load_session_fixture(SESSION_WITH_4_TOOL_CALLS);

    let outcome = distill_skill_from_experience_with_learning(
        &MoaConfig::default(),
        &loaded.session,
        experience_input(&loaded, &fixture_task(&loaded)),
        scripted_router(Vec::<String>::new()),
        None,
        None,
        &DispatchEvidence::SingleSession,
    )
    .await
    .expect("skip below threshold");

    assert_eq!(
        outcome,
        DistillationOutcome::Skipped {
            reason: DistillationSkipReason::BelowThreshold
        }
    );
}

#[tokio::test]
async fn failed_experience_does_not_trigger_distillation_even_above_threshold() {
    // Pins: an experience with a failed assessed outcome cannot seed a reusable skill,
    // regardless of how many tool calls the segment contains.
    let loaded = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    let mut input = experience_input(&loaded, &fixture_task(&loaded));
    input.experience.outcome = SegmentOutcome::Failed;

    let outcome = distill_skill_from_experience_with_learning(
        &MoaConfig::default(),
        &loaded.session,
        input,
        scripted_router(Vec::<String>::new()),
        None,
        None,
        &DispatchEvidence::SingleSession,
    )
    .await
    .expect("skip failed experience");

    assert_eq!(
        outcome,
        DistillationOutcome::Skipped {
            reason: DistillationSkipReason::UnlearnableOutcome
        }
    );
}

#[tokio::test]
async fn distillation_above_similarity_threshold_routes_to_improver() {
    let test_db = setup_test_db().await;
    let loaded = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let storage_partition_id = session_storage_partition_id(&loaded.session);
    let scope = tenant_scope(&storage_partition_id);
    let similar = skill_markdown(
        "debug-oauth-refresh-regression",
        "debug oauth refresh regression with bash file search validation",
        "Baseline reusable OAuth refresh workflow.",
        "1.0",
    );
    seed_skill(&test_db, scope, &similar).await;
    let improved = skill_markdown(
        "debug-oauth-refresh-regression",
        "debug oauth refresh regression with bash file search validation",
        "Improved reusable OAuth refresh workflow.",
        "1.0",
    );

    let outcome = distill_skill_from_experience_with_learning(
        &config,
        &loaded.session,
        experience_input(&loaded, &fixture_task(&loaded)),
        scripted_router([improved]),
        Some(learning_store(&test_db)),
        None,
        &DispatchEvidence::SingleSession,
    )
    .await
    .expect("route similar experience to improver");

    let DistillationOutcome::ImprovementProposed {
        existing_skill_id,
        proposal,
    } = outcome
    else {
        panic!("expected improvement proposal");
    };
    assert_eq!(existing_skill_id, "debug-oauth-refresh-regression");
    assert_eq!(
        proposal.expect("stored improvement proposal").metadata.name,
        existing_skill_id
    );
}

#[tokio::test]
async fn distillation_below_similarity_threshold_creates_new_skill() {
    let test_db = setup_test_db().await;
    let loaded = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let storage_partition_id = session_storage_partition_id(&loaded.session);
    let scope = tenant_scope(&storage_partition_id);
    let unrelated = skill_markdown(
        "terraform-state-cleanup",
        "Clean Terraform state for decommissioned services",
        "Use cloud inventory and state commands to remove stale infrastructure.",
        "1.0",
    );
    seed_skill(&test_db, scope, &unrelated).await;
    let proposed = skill_markdown(
        "release-cache-reset",
        "Reset a release cache safely",
        "This workflow is unrelated to auth refresh debugging.",
        "1.0",
    );

    let outcome = distill_skill_from_experience_with_learning(
        &config,
        &loaded.session,
        experience_input(&loaded, &fixture_task(&loaded)),
        scripted_router([proposed]),
        Some(learning_store(&test_db)),
        None,
        &DispatchEvidence::SingleSession,
    )
    .await
    .expect("create unrelated skill");

    let DistillationOutcome::NewSkillProposed { proposal } = outcome else {
        panic!("expected new skill for unrelated summary");
    };
    assert_eq!(proposal.metadata.name, "release-cache-reset");
}

#[tokio::test]
async fn distillation_candidate_includes_lineage_pointers_to_session_and_experience() {
    // Pins: the review candidate records both the originating session and the source
    // experience so promoted learning is auditable back to its evidence.
    let test_db = setup_test_db().await;
    let loaded = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let proposed = skill_markdown(
        "auth-lineage-distilled",
        "Capture lineage for distilled auth sessions",
        "Keep the reusable auth workflow steps concise.",
        "1.0",
    );
    let input = experience_input(&loaded, &fixture_task(&loaded));
    let experience_id = input.experience.id;

    let outcome = distill_skill_from_experience_with_learning(
        &config,
        &loaded.session,
        input,
        scripted_router([proposed]),
        Some(learning_store(&test_db)),
        None,
        &DispatchEvidence::SingleSession,
    )
    .await
    .expect("distill candidate with lineage");

    let DistillationOutcome::NewSkillProposed { proposal } = outcome else {
        panic!("expected lineage proposal");
    };
    let store = learning_store(&test_db);
    let candidate = store
        .get_learning_candidate(&loaded.session.tenant_id, proposal.candidate_id)
        .await
        .expect("load lineage candidate")
        .expect("candidate exists");
    let session_id = loaded.session.id.to_string();
    assert_eq!(
        candidate.payload["source_session_id"]
            .as_str()
            .expect("source_session_id is string"),
        session_id
    );
    assert_eq!(candidate.source_experience_ids, vec![experience_id]);
}

#[tokio::test]
async fn semantic_dedup_accumulates_sibling_instead_of_filing_a_near_duplicate() {
    // Pins: with an embedder present, a differently-worded experience whose task
    // embedding is within the dedup threshold of an open proposal's source
    // experience dedupes onto that proposal (accumulates a sibling) instead of
    // filing a parallel near-duplicate draft. The probe is embedded for both
    // passes.
    let test_db = setup_test_db().await;
    let loaded = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    let tenant = loaded.session.tenant_id;
    let (config, _temp_dir) = test_config(&test_db);
    let store = learning_store(&test_db);
    let (embedder, embed_calls) = scripted_embedder();

    // Pass 1: file a fresh proposal for experience A.
    let input_a = experience_input(&loaded, "rotate the alpha deploy token safely");
    let experience_a = input_a.experience.id;
    let created = skill_markdown(
        "deploy-token-rotate",
        "Rotate a deploy token and verify the new credential",
        "Follow the rotate-and-verify workflow.",
        "1.0",
    );
    let first = distill_skill_from_experience_with_learning(
        &config,
        &loaded.session,
        input_a,
        scripted_router([created]),
        Some(store.clone()),
        Some(embedder.clone()),
        &DispatchEvidence::SingleSession,
    )
    .await
    .expect("file first proposal");
    let DistillationOutcome::NewSkillProposed { proposal: open } = first else {
        panic!("expected first pass to file a new proposal");
    };

    // Make experience A discoverable by the dedup NN: persist it with the same
    // fixed probe vector the embedder returns, so a later probe sits at distance 0.
    seed_embedded_experience(
        &test_db,
        experience_a,
        tenant,
        "fixture-rotate-the-alpha-deploy-token-safely",
        "rotate the alpha deploy token safely",
        &learning_probe_vector(),
        Utc::now(),
    )
    .await;

    // Pass 2: a differently-worded (distinct-fingerprint) experience. It routes to
    // create, then the semantic dedup finds experience A behind the open proposal
    // and accumulates a sibling instead of filing.
    let input_b = experience_input(&loaded, "cycle the production signing key now");
    let second = distill_skill_from_experience_with_learning(
        &config,
        &loaded.session,
        input_b,
        scripted_router(["UNCHANGED".to_string()]),
        Some(store.clone()),
        Some(embedder.clone()),
        &DispatchEvidence::SingleSession,
    )
    .await
    .expect("dedupe second experience");

    let DistillationOutcome::DedupedOntoOpenProposal { proposal, .. } = second else {
        panic!("expected the near-duplicate to dedupe onto the open proposal");
    };
    assert_eq!(
        proposal.candidate_id, open.candidate_id,
        "the sibling deduped onto the first proposal's candidate"
    );
    // The probe was embedded once per pass; the semantic layer ran.
    assert_eq!(embed_calls.load(Ordering::SeqCst), 2);

    // Still exactly one open skill proposal for the tenant — no parallel draft.
    let open_sources = store
        .list_open_skill_proposal_sources(&tenant)
        .await
        .expect("list open proposal sources");
    assert_eq!(open_sources.len(), 1);
    assert_eq!(open_sources[0].candidate_id, open.candidate_id);
}

#[tokio::test]
async fn absent_embedder_skips_semantic_dedup_and_files_a_new_proposal() {
    // Pins: the embedder gates the whole semantic layer. With no embedder, the
    // same near-duplicate that would dedupe with embeddings present instead files
    // its own proposal via the lexical path — zero embedding work is done.
    let test_db = setup_test_db().await;
    let loaded = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    let tenant = loaded.session.tenant_id;
    let (config, _temp_dir) = test_config(&test_db);
    let store = learning_store(&test_db);

    let input_a = experience_input(&loaded, "rotate the alpha deploy token safely");
    let experience_a = input_a.experience.id;
    let created = skill_markdown(
        "deploy-token-rotate",
        "Rotate a deploy token and verify the new credential",
        "Follow the rotate-and-verify workflow.",
        "1.0",
    );
    distill_skill_from_experience_with_learning(
        &config,
        &loaded.session,
        input_a,
        scripted_router([created]),
        Some(store.clone()),
        None,
        &DispatchEvidence::SingleSession,
    )
    .await
    .expect("file first proposal");
    seed_embedded_experience(
        &test_db,
        experience_a,
        tenant,
        "fixture-rotate-the-alpha-deploy-token-safely",
        "rotate the alpha deploy token safely",
        &learning_probe_vector(),
        Utc::now(),
    )
    .await;

    let input_b = experience_input(&loaded, "cycle the production signing key now");
    let unrelated = skill_markdown(
        "signing-key-cycle",
        "Cycle the production signing key",
        "Follow the key-cycle workflow.",
        "1.0",
    );
    let second = distill_skill_from_experience_with_learning(
        &config,
        &loaded.session,
        input_b,
        scripted_router([unrelated]),
        Some(store.clone()),
        None,
        &DispatchEvidence::SingleSession,
    )
    .await
    .expect("file second proposal on the lexical path");

    assert!(
        matches!(second, DistillationOutcome::NewSkillProposed { .. }),
        "without an embedder the near-duplicate files its own draft"
    );
    let open_sources = store
        .list_open_skill_proposal_sources(&tenant)
        .await
        .expect("list open proposal sources");
    assert_eq!(
        open_sources.len(),
        2,
        "the lexical path filed a second parallel proposal"
    );
}

#[tokio::test]
async fn semantically_close_fingerprint_groups_merge_into_one_recurrence_cluster() {
    // Pins: two exact-fingerprint groups whose task summaries embed close together
    // merge into a single recurrence cluster via the real nearest-neighbor query,
    // so one dispatch (not two) covers both fingerprints, carrying both fingerprint
    // hashes as reviewer evidence.
    let test_db = setup_test_db().await;
    let store = learning_store(&test_db);
    let tenant = TenantId::new();
    let now = Utc::now();

    // Three occurrences of each of two differently-worded-but-equivalent tasks,
    // all embedded to the same probe vector so they cluster.
    for (fingerprint, summary) in [
        ("rotate-token-aaa", "rotate the alpha deploy token"),
        ("rotate-token-bbb", "cycle the alpha deployment token"),
    ] {
        for age in [3_i64, 2, 1] {
            seed_embedded_experience(
                &test_db,
                uuid::Uuid::now_v7(),
                tenant,
                fingerprint,
                summary,
                &learning_probe_vector(),
                now - Duration::days(age),
            )
            .await;
        }
    }

    let since = now - Duration::days(30);
    let groups = store
        .list_candidate_experience_groups(&tenant, since, 200)
        .await
        .expect("group by fingerprint");
    assert_eq!(
        groups.len(),
        2,
        "two exact-fingerprint groups before merging"
    );

    // Probe each group's representative through the real NN, then cluster.
    let mut neighbor_lists = Vec::new();
    for group in &groups {
        let representative = group.members[0].experience_id;
        let neighbors = store
            .nearest_task_embeddings_for_experience(&tenant, representative, 16)
            .await
            .expect("nearest neighbors for representative");
        neighbor_lists.push(neighbors);
    }
    let clusters = cluster_recurrence_groups(&groups, &neighbor_lists, 0.85);
    assert_eq!(clusters.len(), 1, "the two groups merge into one cluster");
    let merged = &clusters[0];
    assert_eq!(merged.members.len(), 6);
    assert_eq!(
        merged.merged_fingerprints,
        vec![
            "rotate-token-aaa".to_string(),
            "rotate-token-bbb".to_string()
        ]
    );

    let thresholds = RecurrenceThresholds::from_config(&RecurrenceConfig::default());
    let plan = qualify_recurrence_cluster(merged, &[], &thresholds, now)
        .expect("merged cluster qualifies for one dispatch");
    assert_eq!(plan.occurrences, 6);
    assert_eq!(
        plan.merged_fingerprints,
        vec![
            "rotate-token-aaa".to_string(),
            "rotate-token-bbb".to_string()
        ]
    );
}

/// Drives the full store→cluster→qualify flow for a set of single-occurrence,
/// mutually-similar fingerprint aliases and returns the qualification outcome.
async fn qualify_sub_threshold_aliases(
    aliases: &[(&str, &str)],
) -> Option<moa_skills::recurrence::RecurrenceDispatchPlan> {
    let test_db = setup_test_db().await;
    let store = learning_store(&test_db);
    let tenant = TenantId::new();
    let now = Utc::now();

    // One occurrence of each differently-worded alias — every alias is below the
    // occurrence floor on its own, all embedded to the same probe so they cluster.
    for (index, (fingerprint, summary)) in aliases.iter().enumerate() {
        seed_embedded_experience(
            &test_db,
            uuid::Uuid::now_v7(),
            tenant,
            fingerprint,
            summary,
            &learning_probe_vector(),
            now - Duration::days(index as i64 + 1),
        )
        .await;
    }

    let since = now - Duration::days(30);
    let groups = store
        .list_candidate_experience_groups(&tenant, since, 200)
        .await
        .expect("candidate groups");
    assert_eq!(
        groups.len(),
        aliases.len(),
        "each single-occurrence alias is a candidate group (below-floor groups are not discarded)"
    );

    let mut neighbor_lists = Vec::new();
    for group in &groups {
        let representative = group.members[0].experience_id;
        neighbor_lists.push(
            store
                .nearest_task_embeddings_for_experience(&tenant, representative, 16)
                .await
                .expect("nearest neighbors"),
        );
    }
    let clusters = cluster_recurrence_groups(&groups, &neighbor_lists, 0.85);
    assert_eq!(clusters.len(), 1, "the aliases merge into one cluster");
    assert_eq!(clusters[0].members.len(), aliases.len());

    let thresholds = RecurrenceThresholds::from_config(&RecurrenceConfig::default());
    qualify_recurrence_cluster(&clusters[0], &[], &thresholds, now)
}

#[tokio::test]
async fn sub_threshold_fingerprint_aliases_merge_and_dispatch_db() {
    // Pins F6: three differently-worded fingerprints, each seen exactly once (all
    // below the occurrence floor of three individually), merge semantically into one
    // cluster that collectively clears the threshold and dispatches — the exact case
    // the old pre-clustering HAVING filter discarded before the merge could run.
    let plan = qualify_sub_threshold_aliases(&[
        ("rotate-token-a", "rotate the alpha deploy token"),
        ("rotate-token-b", "cycle the alpha deployment token"),
        ("rotate-token-c", "refresh the alpha deploy token"),
    ])
    .await
    .expect("three sub-threshold aliases collectively qualify");
    assert_eq!(plan.occurrences, 3);
    assert_eq!(
        plan.merged_fingerprints,
        vec![
            "rotate-token-a".to_string(),
            "rotate-token-b".to_string(),
            "rotate-token-c".to_string(),
        ],
        "every merged alias fingerprint rides the dispatch as reviewer evidence"
    );
}

#[tokio::test]
async fn merged_cluster_still_below_threshold_does_not_dispatch_db() {
    // Pins F6's boundary: two single-occurrence aliases merge into one cluster, but
    // two occurrences is still below the floor of three, so the post-clustering
    // threshold correctly abstains. The threshold moved after the merge; it did not
    // disappear.
    let plan = qualify_sub_threshold_aliases(&[
        ("rotate-token-a", "rotate the alpha deploy token"),
        ("rotate-token-b", "cycle the alpha deployment token"),
    ])
    .await;
    assert!(
        plan.is_none(),
        "a merged cluster below the occurrence floor must not dispatch"
    );
}

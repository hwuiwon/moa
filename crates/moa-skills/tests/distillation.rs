//! Integration tests for successful-session skill distillation.

#![recursion_limit = "256"]

mod support;

use moa_core::MoaConfig;
use moa_skills::distiller::{
    DistillationOutcome, DistillationSkipReason, distill_skill_with_learning,
};
use support::{
    SESSION_WITH_4_TOOL_CALLS, SESSION_WITH_5_TOOL_CALLS, configured_test_db, failed_session,
    learning_store, load_optional_active_skill, load_session_fixture, scripted_router, seed_skill,
    session_workspace_id, skill_markdown, test_config, workspace_scope,
};

#[tokio::test]
async fn session_with_5_tool_calls_and_success_outcome_triggers_distillation() {
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let loaded = load_session_fixture(SESSION_WITH_5_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let proposed = skill_markdown(
        "oauth-refresh-regression",
        "Capture the OAuth refresh regression workflow",
        "Use the same repro, search, patch, and validation flow.",
        "1.0",
    );

    let outcome = distill_skill_with_learning(
        &config,
        &loaded.session,
        &loaded.events,
        scripted_router([proposed]),
        Some(learning_store(&test_db)),
    )
    .await
    .expect("distill successful session");

    let DistillationOutcome::NewSkillProposed { proposal } = outcome else {
        panic!("expected new skill proposal");
    };
    assert_eq!(proposal.metadata.name, "oauth-refresh-regression");
    let workspace_id = session_workspace_id(&loaded.session);
    let scope = workspace_scope(&workspace_id);
    assert!(
        load_optional_active_skill(&test_db, &scope, "oauth-refresh-regression")
            .await
            .is_none()
    );
}

#[tokio::test]
async fn session_with_4_tool_calls_does_not_trigger_distillation() {
    let loaded = load_session_fixture(SESSION_WITH_4_TOOL_CALLS);

    let outcome = distill_skill_with_learning(
        &MoaConfig::default(),
        &loaded.session,
        &loaded.events,
        scripted_router(Vec::<String>::new()),
        None,
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
async fn session_with_failure_outcome_does_not_trigger_distillation_even_above_threshold() {
    let loaded = failed_session(load_session_fixture(SESSION_WITH_5_TOOL_CALLS));

    let outcome = distill_skill_with_learning(
        &MoaConfig::default(),
        &loaded.session,
        &loaded.events,
        scripted_router(Vec::<String>::new()),
        None,
    )
    .await
    .expect("skip failed session");

    assert_eq!(
        outcome,
        DistillationOutcome::Skipped {
            reason: DistillationSkipReason::Failure
        }
    );
}

#[tokio::test]
async fn distillation_above_similarity_threshold_routes_to_improver() {
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let loaded = load_session_fixture(SESSION_WITH_5_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let workspace_id = session_workspace_id(&loaded.session);
    let scope = workspace_scope(&workspace_id);
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

    let outcome = distill_skill_with_learning(
        &config,
        &loaded.session,
        &loaded.events,
        scripted_router([improved]),
        Some(learning_store(&test_db)),
    )
    .await
    .expect("route similar session to improver");

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
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let loaded = load_session_fixture(SESSION_WITH_5_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let workspace_id = session_workspace_id(&loaded.session);
    let scope = workspace_scope(&workspace_id);
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

    let outcome = distill_skill_with_learning(
        &config,
        &loaded.session,
        &loaded.events,
        scripted_router([proposed]),
        Some(learning_store(&test_db)),
    )
    .await
    .expect("create unrelated skill");

    let DistillationOutcome::NewSkillProposed { proposal } = outcome else {
        panic!("expected new skill for unrelated summary");
    };
    assert_eq!(proposal.metadata.name, "release-cache-reset");
}

#[tokio::test]
async fn distilled_skill_includes_lineage_pointer_to_originating_session() {
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let loaded = load_session_fixture(SESSION_WITH_5_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let proposed = skill_markdown(
        "auth-lineage-distilled",
        "Capture lineage for distilled auth sessions",
        "Keep a reproducible pointer to the source session.",
        "1.0",
    );

    let outcome = distill_skill_with_learning(
        &config,
        &loaded.session,
        &loaded.events,
        scripted_router([proposed]),
        Some(learning_store(&test_db)),
    )
    .await
    .expect("distill with lineage");

    let DistillationOutcome::NewSkillProposed { proposal } = outcome else {
        panic!("expected lineage proposal");
    };
    let store = learning_store(&test_db);
    let workspace_id = session_workspace_id(&loaded.session);
    let candidate = store
        .get_learning_candidate(&workspace_id, proposal.candidate_id)
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
}

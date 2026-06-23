//! Integration tests for existing-skill improvement and versioning.

#![recursion_limit = "256"]

mod support;

use moa_skills::improver::{ImprovementResult, improve_skill_with_learning};
use support::{
    BASELINE_SKILL, IMPROVED_SKILL, REGRESSED_SKILL, SESSION_WITH_5_TOOL_CALLS,
    active_semantic_version, artifact_revision_count, configured_test_db, learning_store,
    load_session_fixture, scripted_router, seed_skill, session_workspace_id, skill_row_count,
    test_config, workspace_scope,
};

#[tokio::test]
async fn improver_with_changed_body_bumps_minor_version() {
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let loaded = load_session_fixture(SESSION_WITH_5_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let workspace_id = session_workspace_id(&loaded.session);
    let scope = workspace_scope(&workspace_id);
    let existing = seed_skill(&test_db, scope, BASELINE_SKILL).await;

    let result = improve_skill_with_learning(
        &config,
        &loaded.session,
        &existing,
        &loaded.events,
        scripted_router([IMPROVED_SKILL]),
        Some(learning_store(&test_db)),
    )
    .await
    .expect("improve changed skill");

    let ImprovementResult::Improved {
        previous_version,
        version,
        ..
    } = result
    else {
        panic!("expected accepted improvement");
    };
    assert_eq!(previous_version, "1.2");
    assert_eq!(version, "1.3");
    assert_eq!(
        active_semantic_version(&test_db, &scope, "auth-flow").await,
        "1.2"
    );
    assert_eq!(
        skill_row_count(&test_db, &workspace_id, "auth-flow").await,
        1
    );
}

#[tokio::test]
async fn improver_with_unchanged_body_returns_unchanged_short_circuit() {
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let loaded = load_session_fixture(SESSION_WITH_5_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let workspace_id = session_workspace_id(&loaded.session);
    let scope = workspace_scope(&workspace_id);
    let existing = seed_skill(&test_db, scope, BASELINE_SKILL).await;

    let result = improve_skill_with_learning(
        &config,
        &loaded.session,
        &existing,
        &loaded.events,
        scripted_router(["UNCHANGED"]),
        Some(learning_store(&test_db)),
    )
    .await
    .expect("unchanged improvement short-circuits");

    assert!(matches!(result, ImprovementResult::Unchanged { .. }));
    assert_eq!(
        active_semantic_version(&test_db, &scope, "auth-flow").await,
        "1.2"
    );
    assert_eq!(
        skill_row_count(&test_db, &workspace_id, "auth-flow").await,
        1
    );
}

#[tokio::test]
async fn improver_with_breaking_changes_to_skill_signature_bumps_major_version() {
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let loaded = load_session_fixture(SESSION_WITH_5_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let workspace_id = session_workspace_id(&loaded.session);
    let scope = workspace_scope(&workspace_id);
    let existing = seed_skill(&test_db, scope, BASELINE_SKILL).await;

    let result = improve_skill_with_learning(
        &config,
        &loaded.session,
        &existing,
        &loaded.events,
        scripted_router([REGRESSED_SKILL]),
        Some(learning_store(&test_db)),
    )
    .await
    .expect("breaking signature change bumps major version");

    let ImprovementResult::Improved { version, .. } = result else {
        panic!("expected accepted breaking-signature improvement");
    };
    assert_eq!(version, "2.0");
    assert_eq!(
        active_semantic_version(&test_db, &scope, "auth-flow").await,
        "1.2"
    );
}

#[tokio::test]
async fn improver_concurrent_attempts_on_same_skill_reuse_draft_proposal() {
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let loaded = load_session_fixture(SESSION_WITH_5_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let workspace_id = session_workspace_id(&loaded.session);
    let scope = workspace_scope(&workspace_id);
    let existing = seed_skill(&test_db, scope, BASELINE_SKILL).await;
    let router = scripted_router([
        IMPROVED_SKILL,
        IMPROVED_SKILL,
        IMPROVED_SKILL,
        IMPROVED_SKILL,
        IMPROVED_SKILL,
    ]);
    let store = learning_store(&test_db);

    let result_1 = improve_skill_with_learning(
        &config,
        &loaded.session,
        &existing,
        &loaded.events,
        router.clone(),
        Some(store.clone()),
    );
    let result_2 = improve_skill_with_learning(
        &config,
        &loaded.session,
        &existing,
        &loaded.events,
        router.clone(),
        Some(store.clone()),
    );
    let result_3 = improve_skill_with_learning(
        &config,
        &loaded.session,
        &existing,
        &loaded.events,
        router.clone(),
        Some(store.clone()),
    );
    let result_4 = improve_skill_with_learning(
        &config,
        &loaded.session,
        &existing,
        &loaded.events,
        router.clone(),
        Some(store.clone()),
    );
    let result_5 = improve_skill_with_learning(
        &config,
        &loaded.session,
        &existing,
        &loaded.events,
        router,
        Some(store),
    );

    let results = tokio::join!(result_1, result_2, result_3, result_4, result_5);
    for result in [results.0, results.1, results.2, results.3, results.4] {
        assert!(matches!(
            result.expect("concurrent improvement completes"),
            ImprovementResult::Improved { .. }
        ));
    }
    assert_eq!(
        active_semantic_version(&test_db, &scope, "auth-flow").await,
        "1.2"
    );
    assert_eq!(
        skill_row_count(&test_db, &workspace_id, "auth-flow").await,
        1
    );
    assert_eq!(
        artifact_revision_count(&test_db, &workspace_id, "auth-flow").await,
        2
    );
}

#[tokio::test]
async fn improver_emits_review_candidate_with_lineage_payload() {
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let loaded = load_session_fixture(SESSION_WITH_5_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let workspace_id = session_workspace_id(&loaded.session);
    let scope = workspace_scope(&workspace_id);
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
    .expect("improve and emit lineage");

    let ImprovementResult::Improved { proposal, .. } = result else {
        panic!("expected improvement proposal");
    };
    let candidate = store
        .get_learning_candidate(&workspace_id, proposal.candidate_id)
        .await
        .expect("load improvement candidate")
        .expect("candidate exists");
    let payload = &candidate.payload;
    assert_eq!(payload["previous_version"], "1.2");
    assert_eq!(payload["source_session_id"], loaded.session.id.to_string());
    assert!(
        payload["skill_markdown"]
            .as_str()
            .expect("skill markdown is string")
            .contains("name: auth-flow")
    );
    assert_eq!(
        payload["skill_metadata"]["name"], "auth-flow",
        "payload carries the target skill metadata for review"
    );
    assert_eq!(
        payload["skill_metadata"]["path"], ".moa/skills/auth-flow/SKILL.md",
        "review can materialize the target skill path"
    );
}

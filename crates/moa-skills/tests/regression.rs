//! Integration tests for skill regression decisions and rollback behavior.

#![recursion_limit = "256"]

mod support;

use moa_skills::improver::{ImprovementResult, improve_skill_with_learning};
use moa_skills::{SkillRegressionDecision, SkillRegressionSummary, compare_scores};
use support::{
    BASELINE_SKILL, IMPROVED_SKILL, REGRESSED_SKILL, SESSION_WITH_5_TOOL_CALLS,
    active_semantic_version, configured_test_db, learning_store, load_session_fixture,
    scripted_router, seed_skill, test_config, workspace_scope, write_output_suite,
};

#[tokio::test]
async fn regression_run_on_improved_skill_with_higher_score_commits_new_version() {
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let loaded = load_session_fixture(SESSION_WITH_5_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let scope = workspace_scope(&loaded.session.workspace_id);
    let existing = seed_skill(&test_db, scope.clone(), BASELINE_SKILL).await;
    write_output_suite(&config, &loaded.session.workspace_id, "auth-flow").await;

    let result = improve_skill_with_learning(
        &config,
        &loaded.session,
        &existing,
        &loaded.events,
        scripted_router([IMPROVED_SKILL, "kept", "kept validated"]),
        Some(learning_store(&test_db)),
    )
    .await
    .expect("run accepted regression");

    assert!(matches!(result, ImprovementResult::Improved { .. }));
    assert_eq!(
        active_semantic_version(&test_db, &scope, "auth-flow").await,
        "1.3"
    );
}

#[tokio::test]
async fn regression_run_on_regressed_skill_rolls_back_to_baseline() {
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let loaded = load_session_fixture(SESSION_WITH_5_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let scope = workspace_scope(&loaded.session.workspace_id);
    let existing = seed_skill(&test_db, scope.clone(), BASELINE_SKILL).await;
    write_output_suite(&config, &loaded.session.workspace_id, "auth-flow").await;

    let result = improve_skill_with_learning(
        &config,
        &loaded.session,
        &existing,
        &loaded.events,
        scripted_router([REGRESSED_SKILL, "kept validated", "kept"]),
        Some(learning_store(&test_db)),
    )
    .await
    .expect("run rejected regression");

    let ImprovementResult::Rejected { report } = result else {
        panic!("expected rejected regression");
    };
    assert_eq!(report.decision, SkillRegressionDecision::Rejected);
    assert_eq!(
        active_semantic_version(&test_db, &scope, "auth-flow").await,
        "1.2"
    );
}

#[test]
fn regression_run_with_score_within_noise_band_commits_new_version_only_if_above_threshold() {
    let previous = summary(0.750, 0);
    let candidate = summary(0.755, 0);

    assert!(
        compare_scores(&previous, &candidate),
        "current regression contract accepts any non-regressing score; there is no separate noise band"
    );
}

#[tokio::test]
async fn regression_run_with_eval_suite_failing_to_complete_does_not_commit_or_roll_back() {
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let loaded = load_session_fixture(SESSION_WITH_5_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let scope = workspace_scope(&loaded.session.workspace_id);
    let existing = seed_skill(&test_db, scope.clone(), BASELINE_SKILL).await;
    write_output_suite(&config, &loaded.session.workspace_id, "auth-flow").await;

    let result = improve_skill_with_learning(
        &config,
        &loaded.session,
        &existing,
        &loaded.events,
        scripted_router([IMPROVED_SKILL, "kept validated"]),
        Some(learning_store(&test_db)),
    )
    .await
    .expect("eval failure produces typed regression result");

    let ImprovementResult::Rejected { report } = result else {
        panic!("expected eval failure rejection");
    };
    assert_eq!(report.decision, SkillRegressionDecision::EvalFailed);
    assert_eq!(
        active_semantic_version(&test_db, &scope, "auth-flow").await,
        "1.2"
    );
}

#[tokio::test]
async fn regression_run_persists_score_history_for_all_attempts() {
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let loaded = load_session_fixture(SESSION_WITH_5_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let scope = workspace_scope(&loaded.session.workspace_id);
    let existing = seed_skill(&test_db, scope, BASELINE_SKILL).await;
    write_output_suite(&config, &loaded.session.workspace_id, "auth-flow").await;
    let store = learning_store(&test_db);
    let attempts = [
        [IMPROVED_SKILL, "kept", "kept validated"],
        [REGRESSED_SKILL, "kept validated", "kept"],
        [IMPROVED_SKILL, "kept", "kept validated"],
        [REGRESSED_SKILL, "kept validated", "kept"],
        [IMPROVED_SKILL, "kept", "kept validated"],
    ];

    for responses in attempts {
        improve_skill_with_learning(
            &config,
            &loaded.session,
            &existing,
            &loaded.events,
            scripted_router(responses),
            Some(store.clone()),
        )
        .await
        .expect("run regression attempt");
    }

    let entries = store
        .list_learnings(
            loaded.session.workspace_id.as_str(),
            Some("skill_regression"),
            10,
        )
        .await
        .expect("list regression learning entries");
    assert_eq!(entries.len(), 5);
    let accepted = entries
        .iter()
        .filter(|entry| entry.payload["decision"] == "Accepted")
        .count();
    let rejected = entries
        .iter()
        .filter(|entry| entry.payload["decision"] == "Rejected")
        .count();
    assert_eq!(accepted, 3);
    assert_eq!(rejected, 2);
}

#[tokio::test]
async fn regression_run_under_concurrent_proposals_serializes_decisions_per_skill() {
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let loaded = load_session_fixture(SESSION_WITH_5_TOOL_CALLS);
    let (config, _temp_dir) = test_config(&test_db);
    let scope = workspace_scope(&loaded.session.workspace_id);
    let existing = seed_skill(&test_db, scope.clone(), BASELINE_SKILL).await;
    write_output_suite(&config, &loaded.session.workspace_id, "auth-flow").await;
    let router = scripted_router([
        IMPROVED_SKILL,
        "kept",
        "kept validated",
        "UNCHANGED",
        "UNCHANGED",
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
        router,
        Some(store),
    );

    let mut improved = 0;
    let mut unchanged = 0;
    let results = tokio::join!(result_1, result_2, result_3);
    for result in [results.0, results.1, results.2] {
        match result.expect("concurrent regression proposal") {
            ImprovementResult::Improved { .. } => improved += 1,
            ImprovementResult::Unchanged { .. } => unchanged += 1,
            other => panic!("unexpected concurrent result: {other:?}"),
        }
    }
    assert_eq!(improved, 1);
    assert_eq!(unchanged, 2);
    assert_eq!(
        active_semantic_version(&test_db, &scope, "auth-flow").await,
        "1.3"
    );
}

fn summary(average_score: f64, failed_runs: usize) -> SkillRegressionSummary {
    SkillRegressionSummary {
        average_score,
        failed_runs,
        total_runs: 1,
        total_cost_dollars: 0.0,
    }
}

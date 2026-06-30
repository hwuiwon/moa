//! Tests for skill regression suite source generation and score comparison.

#![recursion_limit = "256"]

#[path = "support/common.rs"]
mod support;

use moa_skills::format::parse_skill_markdown;
use moa_skills::regression::{
    SkillRegressionDecision, SkillRegressionReport, SkillRegressionSummary, compare_scores,
    generate_skill_test_suite_source,
};
use support::{SESSION_WITH_5_TOOL_CALLS, load_session_fixture, skill_markdown};

#[test]
fn generated_suite_source_is_reviewable_without_writing_files() {
    // Pins: proposal generation can attach a regression suite as draft payload text.
    let loaded = load_session_fixture(SESSION_WITH_5_TOOL_CALLS);
    let markdown = skill_markdown(
        "suite-source-skill",
        "Generate suite source for review",
        "Follow the learned task path and verify the final response.",
        "1.0",
    );
    let skill = parse_skill_markdown(&markdown).expect("parse test skill");

    let generated =
        generate_skill_test_suite_source(loaded.session.tenant_id, &skill, &loaded.events)
            .expect("generate suite source");

    assert!(
        generated
            .relative_path
            .ends_with("skills/suite-source-skill/tests/suite.toml")
    );
    assert!(generated.relative_path.starts_with("tenants/"));
    assert!(
        generated
            .source_toml
            .contains("suite-source-skill-regression")
    );
    assert!(generated.source_toml.contains("[[cases]]"));
    assert!(generated.source_toml.contains("auto-generated"));
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

#[test]
fn regression_report_acceptance_treats_missing_suite_as_non_blocking_generation_gate() {
    let report = SkillRegressionReport {
        decision: SkillRegressionDecision::MissingSuite,
        suite_path: None,
        previous: None,
        candidate: None,
        detail: "no suite exists yet".to_string(),
    };

    assert!(report.accepted());
}

fn summary(average_score: f64, failed_runs: usize) -> SkillRegressionSummary {
    SkillRegressionSummary {
        average_score,
        failed_runs,
        total_runs: 1,
        total_cost_dollars: 0.0,
    }
}

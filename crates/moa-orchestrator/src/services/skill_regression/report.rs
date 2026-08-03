//! Stable JSON report projections for skill regression results.

use moa_eval_core::{EvalStatus, engine::EvalRun};
use moa_skills::regression::SkillRegressionSummary;
use serde_json::{Value, json};

/// Projects the serving skill identity included in the regression report.
pub(super) fn previous_skill_payload(skill: &moa_skills::registry::Skill) -> Value {
    json!({
        "skill_uid": skill.skill_uid,
        "version": skill.version,
        "name": skill.name,
    })
}

/// Collects per-case failure detail so a rejected report explains what failed.
pub(super) fn run_failures_json(run: &EvalRun) -> Value {
    let failures = run
        .results
        .iter()
        .filter(|result| !matches!(result.status, EvalStatus::Passed | EvalStatus::Skipped))
        .map(|result| {
            json!({
                "test_case": result.test_case,
                "status": format!("{:?}", result.status),
                "error": result.error,
            })
        })
        .collect::<Vec<_>>();
    Value::Array(failures)
}

/// Projects aggregate regression metrics into the stable report shape.
pub(super) fn regression_summary_to_json(summary: &SkillRegressionSummary) -> Value {
    json!({
        "average_score": summary.average_score,
        "failed_runs": summary.failed_runs,
        "total_runs": summary.total_runs,
        "total_cost_dollars": summary.total_cost_dollars,
    })
}

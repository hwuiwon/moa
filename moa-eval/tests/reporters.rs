//! Integration tests for eval reporters.

use moa_eval::{
    AgentConfig, EvalMetrics, EvalResult, EvalRun, EvalScore, EvalStatus, JsonReporter, Reporter,
    ScoreValue, TestCase, TestSuite,
};
use tempfile::tempdir;

#[tokio::test]
async fn json_reporter_writes_valid_json() {
    let dir = tempdir().expect("temp dir");
    let output_path = dir.path().join("results.json");
    let reporter = JsonReporter {
        output_path: output_path.clone(),
        pretty: true,
    };
    let suite = TestSuite {
        name: "demo".to_string(),
        cases: vec![TestCase {
            name: "case".to_string(),
            input: "hello".to_string(),
            ..TestCase::default()
        }],
        ..TestSuite::default()
    };
    let configs = vec![AgentConfig {
        name: "baseline".to_string(),
        ..AgentConfig::default()
    }];
    let run = EvalRun {
        suite_name: suite.name.clone(),
        started_at: chrono::Utc::now(),
        completed_at: chrono::Utc::now(),
        results: vec![EvalResult {
            test_case: "case".to_string(),
            agent_config: "baseline".to_string(),
            status: EvalStatus::Passed,
            scores: vec![EvalScore {
                evaluator: "output_match".to_string(),
                name: "output_match".to_string(),
                value: ScoreValue::Numeric(1.0),
                comment: None,
            }],
            metrics: EvalMetrics {
                total_tokens: 15,
                ..EvalMetrics::default()
            },
            ..EvalResult::default()
        }],
        summary: moa_eval::RunSummary {
            total_cases: 1,
            passed: 1,
            total_tokens: 15,
            ..moa_eval::RunSummary::default()
        },
    };

    reporter
        .report(&suite, &configs, &run)
        .await
        .expect("json report");

    let content = tokio::fs::read_to_string(&output_path)
        .await
        .expect("read report");
    let parsed: serde_json::Value = serde_json::from_str(&content).expect("valid json");
    assert_eq!(parsed["suite"]["name"], "demo");
    assert_eq!(parsed["configs"][0]["name"], "baseline");
    assert_eq!(parsed["run"]["summary"]["passed"], 1);
}

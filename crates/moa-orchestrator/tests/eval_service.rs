//! Eval service helper coverage.

use moa_core::WorkspaceId;
use moa_core::wire::{
    EvalRunResponse, EvalRunStatus, EvalRunStatusResponse, EvalSuiteListDocument,
};
use moa_eval_core::{
    AgentConfig, EvalResult, EvalRun, EvalStatus, RunSummary, TestCase, TestSuite,
};
use moa_orchestrator::services::eval::{
    EVAL_COMPARE_SQL, EVAL_SCORES_SQL, EvalServiceError, accepted_eval_run_response,
    hosted_eval_report_artifacts, parse_dataset_items_for_workspace,
    status_response_from_run_response, suite_summaries_from_documents, verify_run_status_workspace,
};
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
async fn hosted_eval_reports_return_terminal_and_json_artifacts() {
    // Pins: hosted eval run honors terminal and JSON report requests without writing client paths on the server.
    let now = chrono::Utc::now();
    let suite = TestSuite {
        name: "regression".to_string(),
        cases: vec![TestCase {
            name: "first".to_string(),
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
        suite_name: "regression".to_string(),
        started_at: now,
        completed_at: now,
        results: vec![EvalResult {
            test_case: "first".to_string(),
            agent_config: "baseline".to_string(),
            status: EvalStatus::Passed,
            ..EvalResult::default()
        }],
        summary: RunSummary {
            total_cases: 1,
            passed: 1,
            ..RunSummary::default()
        },
    };

    let artifacts = hosted_eval_report_artifacts(
        &suite,
        &configs,
        &run,
        &["terminal".to_string(), "json:report.json".to_string()],
        true,
    )
    .await
    .expect("report artifacts should build")
    .expect("report artifacts should be present");

    let terminal = artifacts["terminal"][0]
        .as_str()
        .expect("terminal report should be a string");
    assert!(terminal.contains("Suite: regression"));
    assert_eq!(artifacts["json"][0]["target"], "report.json");
    assert_eq!(
        artifacts["json"][0]["document"]["suite"]["suite"]["name"],
        "regression"
    );
    assert_eq!(
        artifacts["json"][0]["document"]["run"]["summary"]["passed"],
        1
    );
}

#[test]
fn suite_list_summarizes_api_supplied_suite_documents() {
    // Pins: hosted eval suite listing parses caller-supplied suite documents instead of relying on a local command path.
    let summaries = suite_summaries_from_documents(vec![EvalSuiteListDocument {
        source: Some("suites/regression.toml".to_string()),
        body: r#"
[suite]
name = "regression"
description = "Regression suite"
tags = ["smoke", "api"]

[[cases]]
name = "first"
input = "hello"

[cases.expected_output]
contains = ["world"]

[[cases]]
name = "second"
input = "bye"

[cases.expected_output]
contains = ["done"]
"#
        .to_string(),
    }])
    .expect("suite document should parse");

    assert_eq!(summaries.len(), 1);
    assert_eq!(
        summaries[0].source.as_deref(),
        Some("suites/regression.toml")
    );
    assert_eq!(summaries[0].name, "regression");
    assert_eq!(summaries[0].cases, 2);
    assert_eq!(
        summaries[0].description.as_deref(),
        Some("Regression suite")
    );
    assert_eq!(
        summaries[0].tags,
        vec!["smoke".to_string(), "api".to_string()]
    );
}

#[test]
fn dataset_jsonl_items_are_constrained_to_authorized_workspace() {
    // Pins: dataset registration defaults missing item workspaces to the authorized workspace and rejects mismatches.
    let workspace_id = WorkspaceId::new("workspace-a");
    let items = parse_dataset_items_for_workspace(
        &workspace_id,
        Some("golden.jsonl"),
        r#"{"query":"alpha","expected_answer":"a"}
{"workspace_id":"workspace-a","query":"beta","expected_answer":"b"}"#,
    )
    .expect("matching workspace dataset should parse");

    assert_eq!(items.len(), 2);
    assert_eq!(items[0].workspace_id, workspace_id);
    assert_eq!(items[1].workspace_id, WorkspaceId::new("workspace-a"));
    assert_eq!(items[0].query, "alpha");

    let error = parse_dataset_items_for_workspace(
        &WorkspaceId::new("workspace-a"),
        Some("golden.jsonl"),
        r#"{"workspace_id":"workspace-b","query":"leak"}"#,
    )
    .expect_err("mismatched workspace should be rejected");

    match error {
        EvalServiceError::DatasetWorkspaceMismatch {
            line,
            request_workspace_id,
            item_workspace_id,
        } => {
            assert_eq!(line, 1);
            assert_eq!(request_workspace_id, WorkspaceId::new("workspace-a"));
            assert_eq!(item_workspace_id, WorkspaceId::new("workspace-b"));
        }
        other => panic!("expected workspace mismatch error, got {other:?}"),
    }
}

#[test]
fn score_queries_scope_every_run_id_by_workspace() {
    // Pins: score and compare queries constrain every requested run id by request.workspace_id.
    assert!(
        EVAL_SCORES_SQL.contains("WHERE run_id = $1 AND workspace_id = $2"),
        "scores query must scope the run id by workspace"
    );
    assert!(
        EVAL_COMPARE_SQL.contains("WHERE run_id = $1 AND workspace_id = $3"),
        "compare base run must be scoped by workspace"
    );
    assert!(
        EVAL_COMPARE_SQL.contains("WHERE run_id = $2 AND workspace_id = $3"),
        "compare new run must be scoped by workspace"
    );
}

#[test]
fn run_status_rejects_stored_workspace_mismatch() {
    // Pins: run_status does not return workflow state from another workspace.
    let run_id =
        Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("fixture run id parses");
    let response = EvalRunStatusResponse {
        workspace_id: WorkspaceId::new("workspace-b"),
        run_id,
        status: EvalRunStatus::Completed,
        suite_name: Some("suite".to_string()),
        exit_code: Some(0),
        summary: Some(json!({})),
        results: Vec::new(),
        error: None,
    };

    let error = verify_run_status_workspace(&WorkspaceId::new("workspace-a"), &response)
        .expect_err("workspace mismatch should be rejected");

    match error {
        EvalServiceError::RunWorkspaceMismatch {
            run_id: actual_run_id,
            request_workspace_id,
        } => {
            assert_eq!(actual_run_id, run_id);
            assert_eq!(request_workspace_id, WorkspaceId::new("workspace-a"));
        }
        other => panic!("expected run workspace mismatch, got {other:?}"),
    }
}

#[test]
fn terminal_run_response_maps_to_status_response() {
    // Pins: terminal hosted eval results remain observable through run_status.
    let run_id =
        Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("fixture run id parses");
    let response = EvalRunResponse {
        workspace_id: WorkspaceId::new("workspace-a"),
        run_id,
        status: EvalRunStatus::Completed,
        suite_name: "suite".to_string(),
        exit_code: 1,
        summary: json!({"failed": 1}),
        results: vec![json!({"status": "failed"})],
        error: None,
    };

    let status = status_response_from_run_response(&response);

    assert_eq!(status.workspace_id, WorkspaceId::new("workspace-a"));
    assert_eq!(status.run_id, run_id);
    assert_eq!(status.status, EvalRunStatus::Completed);
    assert_eq!(status.suite_name.as_deref(), Some("suite"));
    assert_eq!(status.exit_code, Some(1));
    assert_eq!(status.results, vec![json!({"status": "failed"})]);
}

#[test]
fn accepted_run_response_is_non_terminal() {
    // Pins: Eval/run returns an accepted run id rather than terminal eval results.
    let run_id =
        Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("fixture run id parses");
    let response =
        accepted_eval_run_response(WorkspaceId::new("workspace-a"), run_id, "suite".to_string());

    assert_eq!(response.workspace_id, WorkspaceId::new("workspace-a"));
    assert_eq!(response.run_id, run_id);
    assert_eq!(response.status, EvalRunStatus::Running);
    assert_eq!(response.suite_name, "suite");
    assert!(response.results.is_empty());
    assert!(response.error.is_none());
}

//! CLI unit tests.

use super::memory_graph::{GraphStore, NodeLabel, NodeWriteIntent, PiiClass};
use super::memory_ingest::IngestApplyReport;
use super::{
    apply_config_update, default_log_path, doctor_report, eval_exit_code,
    format_cli_ingest_section, memory_ingest_report, memory_search_report, memory_show_report,
    parse_bool, synthesize_cli_ingest_turn, version_text,
};
use chrono::Utc;
use moa_core::{MoaConfig, SessionId, WorkspaceId};
use moa_eval::{EvalRun, EvalStatus, RunSummary};
use serde_json::json;
use tempfile::tempdir;
use tokio::fs;
use uuid::Uuid;

#[test]
fn version_command_uses_package_version() {
    assert_eq!(version_text(), format!("moa {}", env!("CARGO_PKG_VERSION")));
}

#[test]
fn config_updates_known_keys() {
    let mut config = MoaConfig::default();
    apply_config_update(&mut config, "general.default_model", "claude-sonnet-4-6")
        .expect("update config");
    assert_eq!(config.general.default_model, "claude-sonnet-4-6");
    apply_config_update(&mut config, "database.max_connections", "5")
        .expect("update max connections");
    assert_eq!(config.database.max_connections, 5);
    apply_config_update(&mut config, "metrics.enabled", "true").expect("enable metrics");
    apply_config_update(&mut config, "metrics.listen", "127.0.0.1:19090")
        .expect("set metrics listen");
    assert!(config.metrics.enabled);
    assert_eq!(config.metrics.listen, "127.0.0.1:19090");
}

#[test]
fn parse_bool_accepts_common_values() {
    assert!(parse_bool("yes").expect("bool"));
    assert!(!parse_bool("0").expect("bool"));
}

#[tokio::test]
async fn doctor_report_includes_log_file_path() {
    let dir = tempdir().expect("temp dir");
    let base = dir.keep();
    let mut config = MoaConfig::default();
    config.local.memory_dir = base.join("memory").display().to_string();
    config.local.sandbox_dir = base.join("sandbox").display().to_string();
    config.daemon.socket_path = base.join("daemon.sock").display().to_string();
    config.daemon.pid_file = base.join("daemon.pid").display().to_string();
    config.daemon.log_file = base.join("daemon.log").display().to_string();
    config.daemon.auto_connect = false;

    let report = doctor_report(&config, &default_log_path())
        .await
        .expect("doctor report");
    assert!(report.contains("log_file: "));
    assert!(report.contains("Metrics endpoint: disabled"));
    assert!(
        report.contains("--debug to enable")
            || report.contains("set via --debug/--log-file or RUST_LOG")
    );
}

#[tokio::test]
async fn doctor_report_uses_custom_log_file_path() {
    let dir = tempdir().expect("temp dir");
    let base = dir.keep();
    let mut config = MoaConfig::default();
    config.local.memory_dir = base.join("memory").display().to_string();
    config.local.sandbox_dir = base.join("sandbox").display().to_string();
    config.daemon.socket_path = base.join("daemon.sock").display().to_string();
    config.daemon.pid_file = base.join("daemon.pid").display().to_string();
    config.daemon.log_file = base.join("daemon.log").display().to_string();
    config.daemon.auto_connect = false;

    let custom_log = base.join("custom.log");
    let report = doctor_report(&config, &custom_log)
        .await
        .expect("doctor report");
    assert!(report.contains(&format!("log_file: {}", custom_log.display())));
    assert!(report.contains("Metrics endpoint: disabled"));
}

#[test]
fn ci_exit_code_distinguishes_failures_and_errors() {
    let mut run = EvalRun {
        suite_name: "suite".to_string(),
        started_at: chrono::Utc::now(),
        completed_at: chrono::Utc::now(),
        results: Vec::new(),
        summary: RunSummary::default(),
    };

    assert_eq!(eval_exit_code(true, &run), 0);

    run.results.push(moa_eval::EvalResult {
        status: EvalStatus::Failed,
        ..moa_eval::EvalResult::default()
    });
    assert_eq!(eval_exit_code(true, &run), 1);

    run.results.push(moa_eval::EvalResult {
        status: EvalStatus::Error,
        ..moa_eval::EvalResult::default()
    });
    assert_eq!(eval_exit_code(true, &run), 2);
}

#[test]
fn cli_ingest_turn_carries_workspace_source_and_content() {
    let workspace_id = WorkspaceId::new("workspace-ingest");
    let turn = synthesize_cli_ingest_turn(&workspace_id, "Auth Redesign", "Fact: auth uses JWT");

    assert_eq!(turn.workspace_id, workspace_id);
    assert_eq!(turn.turn_seq, 1);
    assert!(turn.transcript.contains("source: Auth Redesign"));
    assert!(turn.transcript.contains("Fact: auth uses JWT"));
    assert_eq!(turn.dominant_pii_class, "none");
}

#[test]
fn cli_ingest_section_reports_graph_counts() {
    let report = IngestApplyReport {
        inserted: 2,
        superseded: 1,
        skipped: 3,
        failed: 0,
    };

    let section = format_cli_ingest_section(std::path::Path::new("sample.md"), "Sample", &report);

    assert!(section.contains("Ingested \"Sample\" (sample.md)"));
    assert!(section.contains("nodes: inserted=2 superseded=1 skipped=3 failed=0"));
    assert!(section.contains("edges: 0"));
    assert!(section.contains("contradictions: 0"));
}

#[tokio::test]
async fn memory_ingest_report_rejects_name_for_multiple_files() {
    let dir = tempdir().expect("temp dir");
    let base = dir.keep();
    let mut config = MoaConfig::default();
    config.database.url = moa_session::testing::test_database_url();
    config.local.memory_dir = base.join("memory").display().to_string();
    config.local.sandbox_dir = base.join("sandbox").display().to_string();

    let first = base.join("a.md");
    let second = base.join("b.md");
    fs::write(&first, "# A").await.expect("write first");
    fs::write(&second, "# B").await.expect("write second");

    let error = memory_ingest_report(&config, &[first, second], Some("Shared"), None)
        .await
        .expect_err("batch ingest with name should fail");
    assert!(error.to_string().contains("--name can only be used"));
}

#[tokio::test]
#[ignore = "requires graph test database with AGE, sidecar, and pgvector migrations"]
async fn memory_ingest_report_graph_smoke() {
    let dir = tempdir().expect("temp dir");
    let base = dir.keep();
    let mut config = MoaConfig::default();
    config.database.url = moa_session::testing::test_database_url();
    config.local.sandbox_dir = base.join("sandbox").display().to_string();

    let source_path = base.join("rfc-0042-auth-redesign.md");
    fs::write(
        &source_path,
        "Fact: auth service uses JWT for session tokens",
    )
    .await
    .expect("write source");

    let report = memory_ingest_report(
        &config,
        std::slice::from_ref(&source_path),
        None,
        Some("workspace-ingest"),
    )
    .await
    .expect("memory ingest report");
    assert!(report.contains("nodes:"));
    assert!(report.contains("edges:"));
}

#[tokio::test]
#[ignore = "requires graph test database with AGE, sidecar, and pgvector migrations"]
async fn memory_search_report_empty_graph_smoke() {
    let mut config = MoaConfig::default();
    config.database.url = moa_session::testing::test_database_url();

    let report = memory_search_report(&config, "unlikely-empty-search", 10)
        .await
        .expect("memory search report");

    assert!(report == "no hits\n" || report.starts_with("uid\tlabel\tname\tscore\tsnippet\n"));
}

#[tokio::test]
#[ignore = "requires graph test database with AGE, sidecar, and pgvector migrations"]
async fn memory_show_report_seeded_node_smoke() {
    let mut config = MoaConfig::default();
    config.database.url = moa_session::testing::test_database_url();
    let store = super::load_graph_store(&config)
        .await
        .expect("load graph store");
    let uid = Uuid::now_v7();
    store
        .create_node(NodeWriteIntent {
            uid,
            label: NodeLabel::Fact,
            workspace_id: Some(super::current_workspace_id().to_string()),
            user_id: None,
            scope: "workspace".to_string(),
            name: "seeded cli memory fact".to_string(),
            properties: json!({
                "summary": "seeded cli memory fact",
                "source_session_id": SessionId::new().to_string(),
            }),
            pii_class: PiiClass::None,
            confidence: Some(1.0),
            valid_from: Utc::now(),
            embedding: None,
            embedding_model: None,
            embedding_model_version: None,
            actor_id: "test".to_string(),
            actor_kind: "system".to_string(),
        })
        .await
        .expect("seed graph node");

    let report = memory_show_report(&config, &uid.to_string())
        .await
        .expect("memory show report");

    assert!(report.contains(&format!("uid: {uid}")));
    assert!(report.contains("label: Fact"));
}

//! Load-test harness unit tests.

use crate::*;

fn repo_root() -> PathBuf {
    repo_root_from_manifest_dir(Path::new(env!("CARGO_MANIFEST_DIR")))
}

fn repo_root_from_manifest_dir(manifest_dir: &Path) -> PathBuf {
    let root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| {
            panic!(
                "failed to resolve MOA workspace root by walking two parents up from CARGO_MANIFEST_DIR starting directory {}",
                manifest_dir.display()
            )
        });
    let workspace_manifest = root.join("Cargo.toml");
    let manifest = std::fs::read_to_string(&workspace_manifest).unwrap_or_else(|error| {
        panic!(
            "failed to resolve MOA workspace root by walking two parents up from CARGO_MANIFEST_DIR starting directory {}; could not read expected workspace marker [workspace] in {}: {error}",
            manifest_dir.display(),
            workspace_manifest.display()
        )
    });
    if !manifest.contains("[workspace]") {
        panic!(
            "failed to resolve MOA workspace root by walking two parents up from CARGO_MANIFEST_DIR starting directory {}; expected workspace marker [workspace] in {}",
            manifest_dir.display(),
            workspace_manifest.display()
        );
    }
    root.to_path_buf()
}

fn inspection_files() -> InspectionFiles {
    InspectionFiles {
        summary_file: "Cargo.toml".to_string(),
        detail_file: "docs/02-brain-orchestration.md".to_string(),
    }
}

fn test_options(profile: SessionProfileKind) -> LoadTestOptions {
    LoadTestOptions {
        mode: LoadMode::Mock,
        target: LoadTarget::Local,
        sessions: 4,
        profile,
        inter_message_delay: Duration::from_millis(1),
        turn_timeout: Duration::from_secs(15),
        output: OutputFormat::Json,
        model: None,
        config_path: None,
        workspace_root: Some(repo_root()),
        daemon_socket: None,
        mock_provider_timing: Default::default(),
    }
}

async fn run_custom_mock_loadtest(
    options: LoadTestOptions,
    plans: Vec<SessionPlan>,
) -> Result<LoadTestReport> {
    let mut config = load_config(options.config_path.as_deref())?;
    config.observability.enabled = false;
    config.metrics.enabled = false;
    config.memory.auto_bootstrap = false;
    config.compaction.enabled = false;
    config.session_limits.max_turns = 0;
    config.session_limits.loop_detection_threshold = 0;

    let workspace_root = Some(resolve_workspace_root(options.workspace_root.as_deref())?);
    let backend = build_backend(&options, &mut config, workspace_root.clone()).await?;
    let started = Instant::now();
    let run_result = run_sessions(
        backend.clone(),
        &options,
        plans,
        workspace_root.clone(),
        started,
    )
    .await;
    let cleanup_result = backend.cleanup().await;

    match (run_result, cleanup_result) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
        (Err(error), Err(_cleanup_error)) => Err(error),
    }
}

fn approval_heavy_plans(sessions: usize) -> Vec<SessionPlan> {
    (0..sessions)
        .map(|index| SessionPlan {
            profile: SessionProfileKind::Long,
            title: format!("approval-heavy-{index:04}"),
            turns: vec![
                TurnPlan {
                    prompt: "Summarize the active workspace in one sentence.".to_string(),
                    mock_behavior: MockTurnBehavior::Simple,
                },
                TurnPlan {
                    prompt: "Use bash to print an approval marker before answering.".to_string(),
                    mock_behavior: MockTurnBehavior::Bash {
                        cmd: format!("printf 'approval-{index}-1\\n'"),
                    },
                },
                TurnPlan {
                    prompt: "Report one likely runtime bottleneck.".to_string(),
                    mock_behavior: MockTurnBehavior::Simple,
                },
                TurnPlan {
                    prompt: "Use bash to print a second approval marker.".to_string(),
                    mock_behavior: MockTurnBehavior::Bash {
                        cmd: format!("printf 'approval-{index}-2\\n'"),
                    },
                },
                TurnPlan {
                    prompt: "Give a short readiness recommendation.".to_string(),
                    mock_behavior: MockTurnBehavior::Simple,
                },
            ],
        })
        .collect()
}

#[tokio::test]
async fn mock_short_profile_produces_parseable_report() {
    let report = run_loadtest(test_options(SessionProfileKind::Short))
        .await
        .expect("loadtest report");

    assert_eq!(report.sessions_requested, 4);
    assert_eq!(report.sessions_completed, 4);
    assert_eq!(report.sessions_failed, 0);
    assert!(report.latency_ms.p95 >= report.latency_ms.p50);
    assert_eq!(report.total_cost_cents, 0);

    let json = render_json_report(&report).expect("json report");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse json");
    assert!(parsed.get("latency_ms").is_some());
    assert!(parsed.get("sessions_completed").is_some());
}

#[test]
fn mixed_profile_includes_long_and_short_plans_with_tool_turns() {
    let plans = build_session_plans(4, SessionProfileKind::Mixed, &inspection_files());

    assert_eq!(plans.len(), 4);
    assert_eq!(
        plans
            .iter()
            .filter(|plan| plan.profile == SessionProfileKind::Long)
            .count(),
        1
    );
    assert_eq!(
        plans
            .iter()
            .filter(|plan| plan.profile == SessionProfileKind::Short)
            .count(),
        3
    );
    assert!(
        plans
            .iter()
            .find(|plan| plan.profile == SessionProfileKind::Long)
            .expect("mixed profile includes one long session")
            .turns
            .iter()
            .any(|turn| matches!(turn.mock_behavior, MockTurnBehavior::FileRead { .. }))
    );
}

#[tokio::test]
async fn approval_heavy_sessions_auto_deny_cleanly_under_concurrency() {
    let session_count = 24;
    let options = LoadTestOptions {
        mode: LoadMode::Mock,
        target: LoadTarget::Local,
        sessions: session_count,
        profile: SessionProfileKind::Long,
        inter_message_delay: Duration::ZERO,
        turn_timeout: Duration::from_secs(20),
        output: OutputFormat::Json,
        model: None,
        config_path: None,
        workspace_root: Some(repo_root()),
        daemon_socket: None,
        mock_provider_timing: Default::default(),
    };

    let report = run_custom_mock_loadtest(options, approval_heavy_plans(session_count))
        .await
        .expect("approval-heavy loadtest report");

    assert_eq!(report.sessions_requested, session_count);
    assert_eq!(report.sessions_completed, session_count);
    assert_eq!(report.sessions_failed, 0);
    assert_eq!(report.auto_denied_approvals, session_count * 2);
    assert_eq!(report.error_count, 0);
    assert!(
        report.sessions.iter().all(|session| {
            session.failure_reason.is_none() && session.auto_denied_approvals == 2
        }),
        "approval-heavy sessions should complete after automatic denials"
    );
}

#[test]
fn repo_root_resolves_from_cargo_manifest_dir_without_absolute_paths() {
    let root = repo_root();
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let expected_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("moa-loadtest manifest should live two parents below workspace root");

    assert_eq!(root, expected_root);
    assert!(root.join("crates/moa-loadtest").is_dir());
    let manifest = std::fs::read_to_string(root.join("Cargo.toml"))
        .expect("workspace Cargo.toml should be readable");
    assert!(manifest.contains("[workspace]"));
}

#[test]
fn repo_root_panics_with_actionable_message_when_workspace_marker_missing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let manifest_dir = temp.path().join("crates/moa-loadtest");
    std::fs::create_dir_all(&manifest_dir).expect("create synthetic manifest dir");

    let panic = std::panic::catch_unwind(|| repo_root_from_manifest_dir(&manifest_dir))
        .expect_err("missing workspace marker should panic");
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .expect("panic should carry a string message");

    assert!(message.contains("walking two parents up from CARGO_MANIFEST_DIR"));
    assert!(message.contains(&manifest_dir.display().to_string()));
    assert!(message.contains("[workspace]"));
}

#[tokio::test]
#[ignore = "stress validation for realistic mock traffic"]
async fn mock_short_profile_handles_hundred_sessions_within_throughput_budget() {
    let mut options = test_options(SessionProfileKind::Short);
    options.sessions = 100;
    options.inter_message_delay = Duration::ZERO;
    options.turn_timeout = Duration::from_secs(20);

    let started = Instant::now();
    let report = run_loadtest(options).await.expect("loadtest report");

    assert_eq!(report.sessions_requested, 100);
    assert_eq!(report.sessions_completed, 100);
    assert_eq!(report.sessions_failed, 0);
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "mock mixed profile exceeded 30s: {:?}",
        started.elapsed()
    );
}

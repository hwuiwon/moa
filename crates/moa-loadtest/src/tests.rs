//! Load-test harness unit tests.

use crate::*;

fn repo_root() -> PathBuf {
    PathBuf::from("/Users/hwuiwon/Github/moa")
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

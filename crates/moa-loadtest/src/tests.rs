//! Load-test harness unit tests.

use crate::*;

fn inspection_files() -> InspectionFiles {
    InspectionFiles {
        summary_file: "Cargo.toml".to_string(),
        detail_file: "docs/02-brain-orchestration.md".to_string(),
    }
}

fn test_options() -> LoadTestOptions {
    LoadTestOptions {
        mode: LoadMode::Mock,
        endpoint: "http://localhost:10010".to_string(),
        sessions: 4,
        profile: SessionProfileKind::Short,
        inter_message_delay: Duration::from_millis(1),
        turn_timeout: Duration::from_secs(15),
        output: OutputFormat::Json,
        model: None,
        config_path: None,
    }
}

#[test]
fn load_options_accept_remote_endpoint() {
    // Pins: loadtest validates a Restate ingress endpoint without any local or daemon target mode.
    let options = test_options();

    assert!(options.validate().is_ok());
}

#[test]
fn load_options_reject_empty_endpoint() {
    // Pins: the remote-only target requires an explicit non-empty endpoint URL.
    let mut options = test_options();
    options.endpoint = "   ".to_string();

    let error = options
        .validate()
        .expect_err("empty endpoint should be rejected");

    assert_eq!(
        error.to_string(),
        "validation error: endpoint must be non-empty"
    );
}

#[test]
fn mixed_profile_includes_long_and_short_plans_with_tool_turns() {
    // Pins: mixed load profiles retain one long tool-heavy session per four sessions.
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
            .any(|turn| {
                turn.prompt.contains("Use tools")
                    || turn.prompt.contains("Inspect")
                    || turn.prompt.contains("Read")
            })
    );
}

#[test]
fn human_report_renders_remote_endpoint() {
    // Pins: reports expose the remote Restate ingress endpoint instead of local/daemon target names.
    let report = LoadTestReport {
        mode: LoadMode::Mock,
        endpoint: "http://localhost:10010".to_string(),
        profile: SessionProfileKind::Short,
        sessions_requested: 1,
        sessions_completed: 1,
        sessions_failed: 0,
        error_count: 0,
        total_tool_calls: 0,
        auto_denied_approvals: 0,
        duration_ms: 10.0,
        latency_ms: summarize_percentiles(&[10.0]),
        ttft_ms: summarize_percentiles(&[]),
        cache_hit_rate: summarize_percentiles(&[0.0]),
        total_cost_cents: 0,
        sessions: Vec::new(),
    };

    let rendered = render_human_report(&report);

    assert!(rendered.contains("Endpoint: http://localhost:10010"));
    assert!(!rendered.contains("Target:"));
}

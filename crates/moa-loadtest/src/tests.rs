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
        edge_endpoint: None,
        sessions: 4,
        tenants: 2,
        identities_per_tenant: 1,
        profile: SessionProfileKind::Short,
        think_time: Duration::from_millis(1),
        rate: 20.0,
        shape: LoadShape::Steady,
        rate_end: None,
        spike_factor: 10.0,
        arrival: ArrivalProcess::Constant,
        duration: Duration::from_secs(10),
        warmup: Some(Duration::from_secs(1)),
        turn_timeout: Duration::from_secs(15),
        output: OutputFormat::Json,
        model: None,
        metrics_endpoint: None,
        seed: 42,
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
    let plans: Vec<_> = (0..4)
        .map(|index| session_plan(index, SessionProfileKind::Mixed, &inspection_files()))
        .collect();

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
fn human_report_renders_endpoint_error_taxonomy_and_windows() {
    // Pins: reports expose the remote endpoint, the corrected/uncorrected
    // latency split, the error taxonomy line, and the window series.
    let summary = |value: f64| PercentileSummary {
        min: value,
        mean: value,
        p50: value,
        p95: value,
        p99: value,
        max: value,
    };
    let report = LoadTestReport {
        mode: LoadMode::Mock,
        endpoint: "http://localhost:10010".to_string(),
        profile: SessionProfileKind::Short,
        requested_rate_qps: 20.0,
        achieved_rate_qps: 19.5,
        sessions_started: 1,
        sessions_completed: 1,
        sessions_failed: 0,
        turns_scheduled: 10,
        turns_completed: 9,
        errors: ErrorTaxonomy {
            turn_timeouts: 1,
            ..ErrorTaxonomy::default()
        },
        total_tool_calls: 0,
        auto_denied_approvals: 0,
        duration_ms: 10_000.0,
        warmup_ms: 1_000.0,
        turn_latency_corrected_ms: summary(12.0),
        turn_latency_ms: summary(10.0),
        dispatch_delay_ms: summary(2.0),
        ttft_ms: summary(0.0),
        step_latency_ms: vec![StepLatencyReport {
            step: "pipeline_compile".to_string(),
            sample_count: 1,
            latency_ms: summary(4.0),
        }],
        cache_hit_rate: summary(0.0),
        total_cost_cents: 0,
        windows: vec![WindowReport {
            start_ms: 0.0,
            end_ms: 10_000.0,
            warmup: false,
            turns_completed: 9,
            turn_errors: 1,
            latency_corrected_ms: summary(12.0),
        }],
        sessions: Vec::new(),
    };

    let rendered = render_human_report(&report);

    assert!(rendered.contains("Endpoint: http://localhost:10010"));
    assert!(rendered.contains("pipeline_compile (n=1): p50 4ms  p95 4ms  p99 4ms"));
    assert!(rendered.contains("Turn Latency (corrected, from intended arrival):"));
    assert!(rendered.contains("timeout 1"));
    assert!(rendered.contains("Windows (corrected p95 per 10s):"));
    assert!(!rendered.contains("Target:"));
}

#[test]
fn turn_error_rate_counts_failed_turns_over_scheduled_arrivals() {
    // Pins: the turn error rate denominator is scheduled arrivals, so missed
    // (never-dispatched) turns cannot hide a saturated system.
    let errors = ErrorTaxonomy {
        turn_start_failures: 1,
        turn_timeouts: 2,
        turn_failures: 3,
        turn_cancellations: 4,
        event_load_failures: 100,
        session_setup_failures: 100,
        event_error_events: 100,
        tool_error_events: 100,
    };

    assert_eq!(errors.failed_turns(), 10);
}

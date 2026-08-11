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

fn test_run_manifest() -> LoadTestRunManifest {
    LoadTestRunManifest {
        source_revision: "test-revision".to_string(),
        source_state: "clean".to_string(),
        lane: LoadLane::DirectIngress,
        foreground_database_connections: 20,
        background_database_connections: 1,
        direct_turn_event_append: false,
        compose_project: "test-project".to_string(),
        state_identity: "test-project_moa-restate-data".to_string(),
        hardware_id: "test-hardware".to_string(),
        sessions: 4,
        tenants: 2,
        identities_per_tenant: 1,
        shape: LoadShape::Steady,
        arrival: ArrivalProcess::Constant,
        rate_end_qps: None,
        think_time_ms: 1,
        turn_timeout_ms: 15_000,
        schedule_duration_ms: 10_000,
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
fn execution_admission_report_rendering() {
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
        run_manifest: test_run_manifest(),
        mode: LoadMode::Mock,
        endpoint: "http://localhost:10010".to_string(),
        profile: SessionProfileKind::Short,
        requested_rate_qps: 20.0,
        achieved_rate_qps: 19.5,
        admission_rate_qps: 1.0,
        successful_operation_rate_qps: 20.5,
        sessions_started: 1,
        sessions_completed: 1,
        sessions_failed: 0,
        turns_scheduled: 10,
        turns_completed: 9,
        execution_admissions: 1,
        successful_operations: 10,
        errors: ErrorTaxonomy {
            turn_timeouts: 1,
            turn_cleanup_failures: 1,
            ..ErrorTaxonomy::default()
        },
        total_tool_calls: 0,
        auto_denied_approvals: 0,
        duration_ms: 10_000.0,
        warmup_ms: 1_000.0,
        turn_latency_corrected_ms: summary(12.0),
        turn_latency_ms: summary(10.0),
        execution_admission_latency_corrected_ms: summary(8.0),
        execution_admission_latency_ms: summary(6.0),
        dispatch_delay_ms: summary(2.0),
        ttft_ms: summary(0.0),
        edge_observation_wait_ms: summary(250.0),
        step_latency_ms: vec![StepLatencyReport {
            step: "pipeline_compile".to_string(),
            sample_count: 1,
            latency_ms: summary(4.0),
        }],
        event_append_phase_latency_ms: vec![EventAppendPhaseLatencyReport {
            phase: "lock_session".to_string(),
            sample_count: 2,
            latency_ms: summary(7.0),
        }],
        resource_bill: ResourceBillReport {
            durable_event_rows: 4,
            durable_event_rows_per_successful_operation: 0.4,
            progress_update_rows: 0,
            progress_update_rows_per_successful_operation: 0.0,
            event_rows_by_type: vec![EventAppendTypeReport {
                event_type: "BrainResponse".to_string(),
                rows: 4,
            }],
        },
        capacity_signals: CapacitySignals::default(),
        cache_hit_rate: summary(0.0),
        total_cost_cents: 0,
        windows: vec![WindowReport {
            start_ms: 0.0,
            end_ms: 10_000.0,
            warmup: false,
            turns_completed: 9,
            execution_admissions: 1,
            turn_errors: 1,
            latency_corrected_ms: summary(12.0),
            execution_admission_latency_corrected_ms: summary(8.0),
        }],
        tenant_ids: Vec::new(),
        hdr: None,
        sessions: Vec::new(),
    };

    let rendered = render_human_report(&report);

    assert!(rendered.contains("Endpoint: http://localhost:10010"));
    assert!(rendered.contains("Lane: DirectIngress"));
    assert!(rendered.contains("DB pools: 20 foreground + 1 background"));
    assert!(rendered.contains("pipeline_compile (n=1): p50 4ms  p95 4ms  p99 4ms"));
    assert!(rendered.contains("lock_session (n=2): p50 7ms  p95 7ms  p99 7ms"));
    assert!(rendered.contains("Edge Observation Wait:"));
    assert!(rendered.contains("1.0/s admissions, 20.5/s successful operations"));
    assert!(rendered.contains("9 answers, 1 admissions, 10 successful operations"));
    assert!(rendered.contains(
        "durable event rows: 4 (0.40/successful operation) | ProgressUpdate: 0 (0.00/successful operation)"
    ));
    assert!(rendered.contains("Turn Latency (corrected, from intended arrival):"));
    assert!(rendered.contains("timeout 1"));
    assert!(rendered.contains("cleanup 1"));
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
        turn_cleanup_failures: 6,
        arrivals_dropped: 5,
        event_load_failures: 100,
        session_setup_failures: 100,
        event_error_events: 100,
        tool_error_events: 100,
    };

    assert_eq!(errors.failed_turns(), 15);
}

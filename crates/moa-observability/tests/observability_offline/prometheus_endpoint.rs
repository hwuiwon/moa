//! Isolated process-level coverage for the development Prometheus exporter.

use std::time::Duration;

use moa_config::{MetricsConfig, MetricsExporter, OtlpProtocol};
use moa_core::types::action_policy::{ActionClass, ActionPolicyEffect, ActionReviewStatus};
use moa_observability::{
    SessionEventAppendPhase, TurnLatencyStep, init_metrics, metrics_endpoint_url,
    record_action_review_decision, record_action_review_oldest_pending_age,
    record_action_review_pending_depth, record_action_review_requested, record_approval_wait,
    record_builtin_approval_decision, record_builtin_approval_oldest_pending_age,
    record_builtin_approval_pending_depth, record_cache_hit_rate,
    record_experiment_learning_candidates, record_experiment_run, record_experiment_score_rows,
    record_experiment_trial, record_genai_client_operation_duration,
    record_genai_client_time_to_first_chunk, record_genai_client_token_usage,
    record_memory_operation, record_session_event_append,
    record_session_event_append_phase_duration, record_simulation_cost_cents,
    record_simulation_tokens, record_simulation_turn, record_turn_latency,
    record_turn_step_duration,
};
use opentelemetry_sdk::Resource;
use tokio::net::TcpListener;
use tokio::time::{Instant, sleep};

#[tokio::test]
async fn prometheus_endpoint_exports_recorded_metric_families() {
    // Pins: the real development exporter installs its process-global recorder,
    // serves the configured HTTP endpoint, and exports the registered runtime
    // families. This lives in its own test binary because recorder installation
    // is intentionally process-global and cannot coexist with OTLP installer
    // tests in the library test process.
    let port = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind ephemeral test port")
        .local_addr()
        .expect("local addr")
        .port();
    let config = MetricsConfig {
        exporter: MetricsExporter::Prometheus,
        prometheus_listen: Some(format!("127.0.0.1:{port}")),
    };
    init_metrics(
        &config,
        None,
        OtlpProtocol::Http,
        &std::collections::HashMap::new(),
        Resource::builder().build(),
    )
    .expect("metrics exporter should initialize");

    record_genai_client_operation_duration(
        "mock",
        "gpt-5.4",
        Some("gpt-5.4"),
        None,
        Duration::from_millis(20),
    );
    record_genai_client_token_usage("mock", "gpt-5.4", "gpt-5.4", "input", 8);
    record_genai_client_token_usage("mock", "gpt-5.4", "gpt-5.4", "output", 4);
    record_genai_client_time_to_first_chunk("mock", "gpt-5.4", "gpt-5.4", Duration::from_millis(5));
    record_cache_hit_rate("mock", "gpt-5.4", 0.5);
    record_turn_latency(Duration::from_millis(25));
    record_turn_step_duration(TurnLatencyStep::PipelineCompile, Duration::from_millis(10));
    record_session_event_append("ToolCall");
    record_session_event_append_phase_duration(
        SessionEventAppendPhase::AcquireConnection,
        Duration::from_millis(2),
    );
    record_session_event_append_phase_duration(
        SessionEventAppendPhase::BeginTransaction,
        Duration::from_millis(1),
    );
    record_session_event_append_phase_duration(
        SessionEventAppendPhase::LockSession,
        Duration::from_millis(3),
    );
    record_memory_operation("search", "ok");
    record_experiment_run("accepted", "agent_loop");
    record_experiment_trial("completed", Some("max_turns"), "agent_loop");
    record_simulation_turn("agent_loop");
    record_simulation_tokens("simulator", 16);
    record_simulation_cost_cents("simulator", 1);
    record_experiment_score_rows("scores", 3);
    record_experiment_learning_candidates("proposed", 1);
    record_action_review_requested(ActionPolicyEffect::AdminReview, ActionClass::LocalWrite);
    record_action_review_decision(ActionReviewStatus::Cleared, ActionClass::LocalWrite);
    record_action_review_decision(ActionReviewStatus::Timeout, ActionClass::CommandExecution);
    record_approval_wait(ActionClass::LocalWrite, Duration::from_secs(30));
    record_action_review_pending_depth(&[("high".to_string(), 2), ("low".to_string(), 1)]);
    record_action_review_oldest_pending_age(42.0);
    record_builtin_approval_pending_depth(3);
    record_builtin_approval_oldest_pending_age(7.0);
    record_builtin_approval_decision("timeout");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("http client");
    let url = metrics_endpoint_url(&config).expect("metrics url");
    let deadline = Instant::now() + Duration::from_secs(5);
    let scrape = loop {
        match client.get(&url).send().await {
            Ok(response) if response.status().is_success() => {
                break response.text().await.expect("scrape body");
            }
            Ok(_) | Err(_) if Instant::now() < deadline => {
                sleep(Duration::from_millis(50)).await;
            }
            Ok(response) => panic!("unexpected scrape status: {}", response.status()),
            Err(error) => panic!("metrics scrape failed: {error}"),
        }
    };

    for family in [
        "gen_ai_client_operation_duration",
        "gen_ai_client_token_usage",
        "gen_ai_client_operation_time_to_first_chunk",
        "moa_cache_hit_rate",
        "moa_turn_latency_seconds",
        "moa_turn_step_duration_seconds",
        "moa_session_events_appended_total",
        "moa_session_event_append_phase_seconds",
        "phase=\"acquire_connection\"",
        "phase=\"begin_transaction\"",
        "moa_memory_operations_total",
        "moa_experiment_runs_total",
        "moa_experiment_trials_total",
        "moa_simulation_turns_total",
        "moa_simulation_tokens_total",
        "moa_simulation_cost_cents_total",
        "moa_experiment_score_rows_total",
        "moa_experiment_learning_candidates_total",
        "moa_action_review_requests_total",
        "moa_action_review_decisions_total",
        "status=\"timeout\"",
        "moa_approval_wait_seconds",
        "moa_action_review_pending",
        "risk_level=\"high\"",
        "risk_level=\"medium\"",
        "moa_action_review_oldest_pending_age_seconds",
        "moa_builtin_approval_pending",
        "moa_builtin_approval_oldest_pending_age_seconds",
        "moa_builtin_approval_decisions_total",
    ] {
        assert!(
            scrape.contains(family),
            "Prometheus scrape should contain `{family}`:\n{scrape}"
        );
    }

    for removed in [
        "moa_session_event_loads_total",
        "moa_context_pipeline_construction_seconds",
        "moa_retrieval_embedder_construction_seconds",
        "moa_tool_idempotency_scan_seconds",
        "moa_memory_operation_duration_seconds",
        "moa_memory_operation_results_total",
        "moa_experiment_trial_duration_seconds",
        "moa_skill_learning_time_in_review_seconds",
    ] {
        assert!(
            !scrape.contains(removed),
            "removed metric family `{removed}` must stay absent:\n{scrape}"
        );
    }
}

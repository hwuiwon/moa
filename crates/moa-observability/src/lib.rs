//! Runtime metrics, tracing bootstrap, and Restate observability helpers.

pub mod restate_observability;
pub mod runtime_metrics;
pub mod telemetry;
pub mod trace_context;
pub mod turn_latency;

#[cfg(test)]
mod test_capture;

pub use restate_observability::current_trace_id;
pub use runtime_metrics::{
    SESSION_EVENT_APPEND_PHASE_METRIC, SessionEventAppendPhase, TURN_LATENCY_REPORT_STEPS,
    TURN_STEP_DURATION_METRIC, TurnLatencyStep, init_metrics, metrics_endpoint_url,
    record_action_review_decision, record_action_review_requested,
    record_api_key_validation_duration, record_cache_hit_rate,
    record_context_pipeline_construction, record_experiment_learning_candidates,
    record_experiment_run, record_experiment_score_rows, record_experiment_trial,
    record_experiment_trial_duration, record_genai_client_operation_duration,
    record_genai_client_time_to_first_chunk, record_genai_client_token_usage,
    record_knowledge_retrieval_duration, record_knowledge_retrieval_hits,
    record_knowledge_sync_run, record_llm_cost_cents, record_memory_operation,
    record_query_rewrite_decision, record_retrieval_embedder_construction,
    record_sandbox_provision_duration, record_session_created, record_session_error,
    record_session_event_append, record_session_event_append_phase_duration,
    record_session_event_decoded_bytes, record_session_event_load, record_sessions_active,
    record_simulation_cost_cents, record_simulation_tokens, record_simulation_turn,
    record_tool_call, record_tool_failure, record_tool_idempotency_scan,
    record_tool_output_truncated_metric, record_tool_reprovision, record_tool_retry,
    record_turn_completed, record_turn_latency, record_turn_step_duration,
    record_turn_workflow_outcome,
};
pub use telemetry::{TelemetryConfig, TelemetryGuard, init_observability};
pub use trace_context::apply_trace_context_to_span;
pub use turn_latency::{
    TurnLatencyCounters, TurnLatencySnapshot, current_turn_root_span, record_turn_compaction,
    record_turn_event_persist_duration, record_turn_llm_call_duration, record_turn_llm_ttft,
    record_turn_pipeline_compile_duration, record_turn_snapshot_load,
    record_turn_snapshot_write_duration, record_turn_tool_dispatch_duration,
    scope_turn_latency_counters,
};

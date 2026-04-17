//! Context compilation and shared harness support utilities.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use moa_core::{
    ApprovalDecision, BufferedUserMessage, CacheReport, CompletionRequest, CompletionResponse,
    ContextSnapshot, Event, EventRange, EventRecord, LLMProvider, MoaError, Result, SessionId,
    SessionMeta, SessionStore, TokenPricing, TraceContext, UserMessage, WorkingContext,
    current_turn_root_span, record_pipeline_compile_duration, record_turn_compaction,
    record_turn_event_persist_duration, record_turn_pipeline_compile_duration,
    record_turn_snapshot_write_duration, stable_prefix_fingerprint,
};
use moa_security::inject_canary;
use tokio::sync::broadcast;
use tracing::Instrument;

use crate::pipeline::ContextPipeline;
use crate::pipeline::history::HISTORY_SNAPSHOT_METADATA_KEY;
use crate::pipeline::runtime_context::WORKSPACE_ROOT_METADATA_KEY;

/// Inputs required to compile one turn's working context.
pub(super) struct BuildTurnContextOptions<'a> {
    pub session_id: &'a SessionId,
    pub session: &'a SessionMeta,
    pub session_store: &'a Arc<dyn SessionStore>,
    pub pipeline: &'a ContextPipeline,
    pub llm_provider: &'a Arc<dyn LLMProvider>,
    pub workspace_root: Option<PathBuf>,
    pub enable_canary: bool,
    pub trace_context: &'a TraceContext,
    pub snapshot_max_size_bytes: usize,
}

/// Runs the context pipeline and persists the latest reusable history snapshot.
pub(super) async fn build_turn_context(
    options: BuildTurnContextOptions<'_>,
) -> Result<(WorkingContext, Option<String>)> {
    let mut ctx = WorkingContext::new(options.session, options.llm_provider.capabilities());
    if let Some(workspace_root) = options.workspace_root {
        ctx.insert_metadata(
            WORKSPACE_ROOT_METADATA_KEY,
            serde_json::json!(workspace_root.display().to_string()),
        );
    }
    let stage_reports = options.pipeline.run(&mut ctx).await?;
    let pipeline_compile_duration = stage_reports.iter().fold(Duration::ZERO, |total, report| {
        total + report.output.duration
    });
    record_pipeline_compile_duration(pipeline_compile_duration);
    record_turn_pipeline_compile_duration(pipeline_compile_duration);
    let active_canary = if options.enable_canary {
        Some(inject_canary(&mut ctx))
    } else {
        None
    };
    ctx.insert_metadata(
        "_moa.session_id",
        serde_json::json!(options.trace_context.session_id.to_string()),
    );
    ctx.insert_metadata(
        "_moa.user_id",
        serde_json::json!(options.trace_context.user_id.to_string()),
    );
    ctx.insert_metadata(
        "_moa.workspace_id",
        serde_json::json!(options.trace_context.workspace_id.to_string()),
    );
    ctx.insert_metadata(
        "_moa.model",
        serde_json::json!(options.trace_context.model.clone()),
    );
    if let Some(platform) = options.trace_context.platform.as_ref() {
        ctx.insert_metadata("_moa.platform", serde_json::json!(platform.to_string()));
    }
    if let Some(trace_name) = options.trace_context.trace_name.as_ref() {
        ctx.insert_metadata("_moa.trace_name", serde_json::json!(trace_name));
    }
    tracing::info!(
        session_id = %options.session_id,
        compiled_messages = ctx.messages.len(),
        total_tokens = ctx.token_count,
        stages = stage_reports.len(),
        pipeline_compile_ms = pipeline_compile_duration.as_millis() as u64,
        "compiled context for streamed brain turn"
    );

    if let Some(report) = stage_reports
        .iter()
        .find(|report| report.name == "compactor")
    {
        let tier1 = report
            .output
            .metadata
            .get("tier1_applied")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let tier2 = report
            .output
            .metadata
            .get("tier2_applied")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let tier3 = report
            .output
            .metadata
            .get("tier3_applied")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let tokens_reclaimed = report
            .output
            .metadata
            .get("tokens_reclaimed")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize;
        let messages_elided = report
            .output
            .metadata
            .get("messages_elided")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize;
        record_turn_compaction(tier1, tier2, tier3, tokens_reclaimed, messages_elided);
    }

    persist_context_snapshot(options.session_store, &ctx, options.snapshot_max_size_bytes).await;

    Ok((ctx, active_canary))
}

async fn persist_context_snapshot(
    session_store: &Arc<dyn SessionStore>,
    ctx: &WorkingContext,
    snapshot_max_size_bytes: usize,
) {
    let Some(snapshot_value) = ctx.metadata().get(HISTORY_SNAPSHOT_METADATA_KEY).cloned() else {
        return;
    };
    if snapshot_value.is_null() {
        let started_at = Instant::now();
        if let Err(error) = session_store.delete_snapshot(ctx.session_id).await {
            tracing::warn!(
                session_id = %ctx.session_id,
                error = %error,
                "compiled context snapshot delete failed"
            );
            return;
        }

        record_turn_snapshot_write_duration(started_at.elapsed());
        return;
    }

    let mut snapshot = match serde_json::from_value::<ContextSnapshot>(snapshot_value) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::warn!(
                session_id = %ctx.session_id,
                error = %error,
                "failed to deserialize compiled context snapshot metadata"
            );
            return;
        }
    };
    snapshot.cache_controls = ctx.cache_controls.clone();

    let serialized = match serde_json::to_vec(&snapshot) {
        Ok(payload) => payload,
        Err(error) => {
            tracing::warn!(
                session_id = %ctx.session_id,
                error = %error,
                "failed to serialize compiled context snapshot"
            );
            return;
        }
    };
    if serialized.len() > snapshot_max_size_bytes {
        tracing::warn!(
            session_id = %ctx.session_id,
            snapshot_bytes = serialized.len(),
            max_snapshot_bytes = snapshot_max_size_bytes,
            "compiled context snapshot exceeded expected size"
        );
    }

    let started_at = Instant::now();
    if let Err(error) = session_store
        .put_snapshot(ctx.session_id, snapshot)
        .await
    {
        tracing::warn!(
            session_id = %ctx.session_id,
            error = %error,
            "compiled context snapshot persist failed; next turn will fall back to replay"
        );
        return;
    }

    record_turn_snapshot_write_duration(started_at.elapsed());
}

pub(super) fn buffer_queued_message(
    queued_messages: &mut Vec<BufferedUserMessage>,
    message: UserMessage,
) {
    queued_messages.push(BufferedUserMessage::direct(message));
}

pub(super) fn turn_number_for_events(events: &[EventRecord]) -> i64 {
    events
        .iter()
        .filter(|record| matches!(record.event, Event::BrainResponse { .. }))
        .count() as i64
        + 1
}

pub(super) fn last_user_message_text(events: &[EventRecord]) -> Option<&str> {
    events.iter().rev().find_map(|record| match &record.event {
        Event::UserMessage { text, .. } | Event::QueuedMessage { text, .. } => Some(text.as_str()),
        _ => None,
    })
}

pub(super) fn record_turn_span_metrics(
    span: &tracing::Span,
    tool_calls: usize,
    input_tokens: usize,
    output_tokens: usize,
    result: &str,
) {
    span.record("moa.turn.tool_calls", tool_calls as i64);
    span.record("moa.turn.input_tokens", input_tokens as i64);
    span.record("moa.turn.output_tokens", output_tokens as i64);
    span.record("moa.turn.result", result);
}

pub(super) fn calculate_response_cost_cents(
    response: &moa_core::CompletionResponse,
    pricing: &TokenPricing,
) -> u32 {
    let usage = response.token_usage();
    let cached_input_tokens = usage.input_tokens_cache_read.min(response.input_tokens);
    let uncached_input_tokens = response.input_tokens.saturating_sub(cached_input_tokens);
    let cached_input_rate = pricing
        .cached_input_per_mtok
        .unwrap_or(pricing.input_per_mtok);

    let cost_dollars = ((uncached_input_tokens as f64 * pricing.input_per_mtok)
        + (cached_input_tokens as f64 * cached_input_rate)
        + (response.output_tokens as f64 * pricing.output_per_mtok))
        / 1_000_000.0;

    (cost_dollars * 100.0).round() as u32
}

pub(super) fn build_cache_report(
    events: &[EventRecord],
    provider: &str,
    request: &CompletionRequest,
    response: &CompletionResponse,
) -> CacheReport {
    let previous_stable_prefix = events.iter().rev().find_map(|record| match &record.event {
        Event::CacheReport { report } => Some(report.stable_prefix_fingerprint),
        _ => None,
    });
    let stable_prefix_fingerprint = stable_prefix_fingerprint(request);
    let stable_prefix_reused = previous_stable_prefix
        .map(|fingerprint| fingerprint == stable_prefix_fingerprint)
        .unwrap_or(false);

    CacheReport::from_request(
        request,
        provider.to_string(),
        response.model.clone(),
        stable_prefix_reused,
        response.token_usage().total_input_tokens(),
        response.token_usage().input_tokens_cache_read,
        response.output_tokens,
    )
}

pub(super) fn approval_decision_label(decision: &ApprovalDecision) -> &'static str {
    match decision {
        ApprovalDecision::AllowOnce => "allow_once",
        ApprovalDecision::AlwaysAllow { .. } => "always_allow",
        ApprovalDecision::Deny { .. } => "deny",
    }
}

pub(super) async fn append_event(
    session_store: &Arc<dyn moa_core::SessionStore>,
    event_tx: Option<&broadcast::Sender<EventRecord>>,
    session_id: SessionId,
    event: Event,
) -> Result<()> {
    let root_turn_span = current_turn_root_span().unwrap_or_else(tracing::Span::current);
    let persist_span = tracing::info_span!(
        parent: &root_turn_span,
        "event_persist",
        moa.persist.events_written = 1i64,
    );
    let started_at = Instant::now();
    let result = async {
        let sequence_num = session_store.emit_event(session_id, event).await?;
        if let Some(event_tx) = event_tx {
            let mut records = session_store
                .get_events(
                    session_id,
                    EventRange {
                        from_seq: Some(sequence_num),
                        to_seq: Some(sequence_num),
                        event_types: None,
                        limit: Some(1),
                    },
                )
                .await?;
            let record = records.pop().ok_or_else(|| {
                MoaError::StorageError("failed to reload appended event".to_string())
            })?;
            let _ = event_tx.send(record);
        }
        Ok(())
    }
    .instrument(persist_span)
    .await;
    record_turn_event_persist_duration(started_at.elapsed(), 1);
    result
}

#[cfg(test)]
mod tests {
    use moa_core::{CompletionResponse, StopReason, TokenPricing};

    use super::calculate_response_cost_cents;

    #[test]
    fn response_cost_cents_uses_provider_pricing() {
        let response = CompletionResponse {
            text: "done".to_string(),
            content: Vec::new(),
            stop_reason: StopReason::EndTurn,
            model: moa_core::ModelId::new("gpt-5.4"),
            input_tokens: 100_000,
            output_tokens: 10_000,
            cached_input_tokens: 50_000,
            usage: moa_core::TokenUsage {
                input_tokens_uncached: 50_000,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 50_000,
                output_tokens: 10_000,
            },
            duration_ms: 1500,
            thought_signature: None,
        };
        let pricing = TokenPricing {
            input_per_mtok: 2.50,
            output_per_mtok: 15.0,
            cached_input_per_mtok: Some(0.25),
        };

        assert_eq!(calculate_response_cost_cents(&response, &pricing), 29);
    }
}

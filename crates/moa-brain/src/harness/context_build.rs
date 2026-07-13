//! Context compilation and shared harness support utilities.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use moa_core::{
    error::Result, events::Event, session_replay::record_pipeline_compile_duration,
    traits::LLMProvider, traits::SessionStore, types::completion::CompletionRequest,
    types::completion::CompletionResponse, types::contact::SessionActorRef,
    types::context::WorkingContext, types::events_stream::EventRecord,
    types::identifiers::SessionId, types::observability::CacheReport,
    types::observability::TraceContext, types::observability::stable_prefix_fingerprint,
    types::session::SessionMeta, types::snapshot::ContextSnapshot,
};
use moa_lineage_core::TurnId;
use moa_observability::{
    current_turn_root_span, record_turn_compaction, record_turn_event_persist_duration,
    record_turn_pipeline_compile_duration, record_turn_snapshot_write_duration,
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
    pub turn_id: TurnId,
}

/// Runs the context pipeline and persists the latest reusable history snapshot.
pub(super) async fn build_turn_context(
    options: BuildTurnContextOptions<'_>,
) -> Result<(WorkingContext, Option<String>)> {
    let mut ctx = WorkingContext::new(options.session, options.llm_provider.capabilities());
    ctx.insert_metadata(
        "_moa.turn_id",
        serde_json::json!(options.turn_id.0.to_string()),
    );
    insert_trace_context_metadata(&mut ctx, options.trace_context);
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
    insert_trace_context_metadata(&mut ctx, options.trace_context);
    if let Some(platform) = options.trace_context.channel.as_ref() {
        ctx.insert_metadata("_moa.channel", serde_json::json!(platform.to_string()));
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

    if let Some(report) = stage_reports.iter().find(|report| report.name == "history") {
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

    let snapshot = match serde_json::from_value::<ContextSnapshot>(snapshot_value) {
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
    if let Err(error) = session_store.put_snapshot(ctx.session_id, snapshot).await {
        tracing::warn!(
            session_id = %ctx.session_id,
            error = %error,
            "compiled context snapshot persist failed; next turn will fall back to replay"
        );
        return;
    }

    record_turn_snapshot_write_duration(started_at.elapsed());
}

fn insert_trace_context_metadata(ctx: &mut WorkingContext, trace_context: &TraceContext) {
    ctx.insert_metadata(
        "_moa.session_id",
        serde_json::json!(trace_context.session_id.to_string()),
    );
    ctx.insert_metadata(
        "_moa.tenant_id",
        serde_json::json!(trace_context.tenant_id.to_string()),
    );
    if let Some(contact_id) = trace_context.contact_id {
        ctx.insert_metadata("_moa.contact_id", serde_json::json!(contact_id.to_string()));
    }
    ctx.insert_metadata(
        "_moa.actor_id",
        serde_json::json!(trace_actor_id(trace_context)),
    );
    ctx.insert_metadata("_moa.model", serde_json::json!(trace_context.model.clone()));
}

fn trace_actor_id(trace_context: &TraceContext) -> String {
    if let Some(contact_id) = trace_context.contact_id {
        return format!("contact:{contact_id}");
    }

    match &trace_context.created_by {
        Some(SessionActorRef::Identity { id }) => format!("identity:{id}"),
        Some(SessionActorRef::Contact { id }) => format!("contact:{id}"),
        Some(SessionActorRef::Anonymous) | None => {
            format!("session:{}", trace_context.session_id)
        }
    }
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

pub(super) fn build_cache_report(
    events: &[EventRecord],
    provider: &str,
    request: &CompletionRequest,
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
        request
            .model
            .clone()
            .unwrap_or_else(|| moa_core::types::identifiers::ModelId::new("")),
        stable_prefix_reused,
        0,
        0,
        0,
    )
}

pub(super) fn complete_cache_report(
    mut report: CacheReport,
    response: &CompletionResponse,
) -> CacheReport {
    let usage = response.token_usage();
    report.model = response.model.clone();
    report.input_tokens = usage.total_input_tokens();
    report.cached_input_tokens = usage.input_tokens_cache_read;
    report.output_tokens = usage.output_tokens;
    report.cached_vs_stable_estimate_ratio = if report.stable_total_tokens_estimate == 0 {
        0.0
    } else {
        usage.input_tokens_cache_read as f64 / report.stable_total_tokens_estimate as f64
    };
    report
}

pub(super) async fn append_event(
    session_store: &Arc<dyn moa_core::traits::SessionStore>,
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
        let record = session_store
            .emit_event_record(session_id, event, None)
            .await?;
        if let Some(event_tx) = event_tx {
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
    use moa_core::{
        types::completion::CompletionResponse, types::completion::StopReason,
        types::model::TokenPricing,
    };

    // Pins: the streaming harness prices responses through the single canonical
    // `TokenPricing::cost_cents` formula (no divergent brain copy). Breaking the
    // formula — e.g. charging cache-write tokens at the standard input rate —
    // changes this expected cent value.
    #[test]
    fn response_cost_cents_uses_canonical_pricing() {
        let response = CompletionResponse {
            text: "done".to_string(),
            content: Vec::new(),
            stop_reason: StopReason::EndTurn,
            model: moa_core::types::identifiers::ModelId::new("gpt-5.4"),
            usage: moa_core::types::completion::TokenUsage {
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
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
        };

        // 50_000 uncached @ 2.50 + 50_000 cache-read @ 0.25 + 10_000 output @ 15.0
        // = 0.125 + 0.0125 + 0.15 = 0.2875 USD -> 29 cents.
        assert_eq!(pricing.cost_cents(&response.token_usage()), 29);
        // Cents and micros derive from the same dollar figure and must agree.
        assert_eq!(pricing.cost_micros(&response.token_usage()), 287_500);
    }
}

//! Durable turn reporting, bounded telemetry, and session outcome delivery.

use std::collections::HashMap;

use moa_brain::pipeline::skills::SELECTED_SKILL_NAMES_METADATA_KEY;
use moa_core::{
    coordination_counters::CoordinationSnapshot,
    events::Event,
    session_replay::TurnReplaySnapshot,
    types::{completion::CompletionResponse, identifiers::SessionId, session::SessionMeta},
};
use moa_observability::{record_session_error, restate_observability::session_turn_span};
use moa_wire::{
    session_store::{
        AppendEventRequest, RecordSegmentSkillActivationRequest, RecordSegmentTurnUsageRequest,
    },
    turn::TurnOutcome,
};
use restate_sdk::prelude::*;

use crate::{
    objects::session::SessionClient,
    restate_identity::{replay_safe_request, with_identity_headers},
    services::session_store::RestateSessionStoreClient,
    turn::util::summarize_response_text,
    workflows::turn_events::{TurnEventAppender, append_session_event},
};

/// Appends a per-turn `TurnMetrics` event when durable metric persistence is enabled.
///
/// Snapshots must be taken before calling so the event does not count its own append.
#[allow(clippy::too_many_arguments)]
pub(super) async fn maybe_append_turn_metrics(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    turn_id: &str,
    actor: &str,
    coordination: &CoordinationSnapshot,
    replay: &TurnReplaySnapshot,
    llm_ms: u64,
    tool_ms: u64,
    persist_ms: u64,
) -> Result<(), HandlerError> {
    if !persist_turn_metrics_enabled() {
        return Ok(());
    }
    replay_safe_request(
        ctx.service_client::<RestateSessionStoreClient>()
            .append_event(Json(AppendEventRequest {
                session_id,
                event: Event::TurnMetrics {
                    turn_id: turn_id.to_string(),
                    actor: actor.to_string(),
                    session_vo_calls: coordination.session_vo_calls,
                    worker_vo_calls: coordination.worker_vo_calls,
                    vo_sends: coordination.vo_sends,
                    durable_appends: coordination.durable_appends,
                    get_events_calls: replay.get_events_calls,
                    events_bytes: replay.events_bytes,
                    llm_ms,
                    tool_ms,
                    persist_ms,
                },
                dedupe_key: Some(format!("turn_metrics:{turn_id}")),
            })),
    )
    .call()
    .await?;
    Ok(())
}

/// Records the visible response summary and active-segment token usage.
pub(super) async fn record_response(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    response: &CompletionResponse,
    last_summary: &mut Option<String>,
) -> Result<(), HandlerError> {
    *last_summary = summarize_response_text(response);
    let usage = response.token_usage();
    let token_cost = (usage.total_input_tokens() + usage.output_tokens) as u64;
    if token_cost > 0 {
        replay_safe_request(
            ctx.service_client::<RestateSessionStoreClient>()
                .record_segment_turn_usage(Json(RecordSegmentTurnUsageRequest {
                    session_id,
                    token_cost,
                })),
        )
        .call()
        .await?;
    }
    Ok(())
}

/// Records each selected segment skill through the durable session store.
pub(super) async fn record_selected_segment_skills(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    metadata: &HashMap<String, serde_json::Value>,
) -> Result<(), HandlerError> {
    for skill_name in selected_skill_names(metadata) {
        replay_safe_request(
            ctx.service_client::<RestateSessionStoreClient>()
                .record_segment_skill_activation(Json(RecordSegmentSkillActivationRequest {
                    session_id,
                    skill_name,
                })),
        )
        .call()
        .await?;
    }
    Ok(())
}

/// Returns the sorted, de-duplicated selected skill names in request metadata.
pub(super) fn selected_skill_names(metadata: &HashMap<String, serde_json::Value>) -> Vec<String> {
    let mut names = metadata
        .get(SELECTED_SKILL_NAMES_METADATA_KEY)
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
}

/// Builds the user-facing message shown when a turn stops at its model-loop cap.
///
/// `max_turns` is the cap actually in force, which is the higher delegation cap when
/// the turn escalated after spawning a worker.
pub(super) fn turn_cap_reached_message(max_turns: usize) -> String {
    format!(
        "MOA stopped because this turn reached the model-loop turn cap ({max_turns}). Narrow the scope or ask MOA to continue."
    )
}

/// Persists the bounded recoverable error emitted when the model-loop cap is reached.
pub(super) async fn emit_turn_cap_exceeded(
    appender: &TurnEventAppender,
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    max_turns: usize,
) -> Result<(), HandlerError> {
    record_session_error("turn_cap");
    append_session_event(
        appender,
        ctx,
        session_id,
        Event::Error {
            message: format!("model-loop turn cap reached ({max_turns}), stopping"),
            recoverable: true,
        },
    )
    .await
    .map(|_| ())
}

/// Creates the root span for one model-loop iteration.
pub(super) fn create_turn_span(
    meta: Option<&SessionMeta>,
    prompt: Option<&str>,
    turn_number: usize,
    environment: Option<&str>,
) -> tracing::Span {
    let Some(meta) = meta else {
        return tracing::info_span!(
            "session_turn",
            otel.name = %format!("MOA turn {turn_number}"),
            moa.turn.number = turn_number as i64,
        );
    };
    session_turn_span(meta, prompt, turn_number as i64, environment)
}

/// Delivers the terminal turn outcome to the owning Session virtual object.
pub(super) async fn notify_session_of_outcome(
    ctx: &WorkflowContext<'_>,
    session_id: &str,
    identity: &moa_core::traits::Identity,
    outcome: &TurnOutcome,
) -> Result<(), HandlerError> {
    moa_core::coordination_counters::record_session_vo_call();
    let request = ctx
        .object_client::<SessionClient>(session_id.to_string())
        .record_turn_outcome(Json::from(outcome.clone()));
    with_identity_headers(request, identity).call().await?;
    tracing::info!(
        session_id = %session_id,
        turn_id = %outcome.turn_id,
        kind = ?outcome.kind,
        "TurnExecution outcome notified to Session VO"
    );
    Ok(())
}

/// Returns whether per-turn `TurnMetrics` events should enter the durable log.
///
/// Off by default and cached once because this environment flag is process-stable
/// across Restate replay.
fn persist_turn_metrics_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("MOA_PERSIST_TURN_METRICS")
            .is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes"))
    })
}

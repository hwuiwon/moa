//! Root-turn response persistence and deferred-ingestion helpers.

use moa_core::{
    events::Event,
    types::completion::{CompletionRequest, CompletionResponse},
    types::events_stream::EventRecord,
    types::identifiers::SessionId,
    types::provider::ModelTier,
    types::session::SessionMeta,
};
use moa_memory_ingest::ingestion_object_key;
use moa_wire::turn::{RunTurnRequest, TurnTrigger};
use restate_sdk::prelude::*;

use crate::objects::ingestion::IngestionVOClient;
use crate::turn_driver::progress as driver_progress;
use crate::workflows::durable_utc_now;
use crate::workflows::turn_events::{
    TurnEventAppender, append_session_event, append_zero_cost_assistant_response,
};

/// Returns whether this run originated from a user message.
pub(super) fn has_user_message_origin(request: &RunTurnRequest) -> bool {
    request.trigger == TurnTrigger::UserMessage
}

/// Returns whether this run synthesizes a durable execution result.
pub(super) fn is_execution_synthesis_turn(request: &RunTurnRequest) -> bool {
    request.trigger == TurnTrigger::ExecutionSynthesis
}

/// Returns whether this run resumes after action review.
pub(super) fn is_action_review_turn(request: &RunTurnRequest) -> bool {
    request.trigger == TurnTrigger::ActionReview
}

/// Persists the deterministic clarification prompt for missing inputs.
pub(super) async fn append_clarification_response(
    appender: &TurnEventAppender,
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    meta: &SessionMeta,
    missing_inputs: &[String],
) -> Result<String, HandlerError> {
    let text = match missing_inputs {
        [] => "What information should I use to continue?".to_string(),
        [field] => format!("I need {field} before I can continue. Please provide it."),
        fields => {
            let fields = fields
                .iter()
                .map(|field| format!("- {field}"))
                .collect::<Vec<_>>()
                .join("\n");
            format!("I need the following information before I can continue:\n\n{fields}")
        }
    };
    append_zero_cost_assistant_response(appender, ctx, session_id, meta, text).await
}

/// Persists one provider completion as a metered brain response.
pub(super) async fn append_brain_response_from_completion(
    appender: &TurnEventAppender,
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    response: &CompletionResponse,
) -> Result<EventRecord, HandlerError> {
    let usage = response.token_usage();
    let cost_cents =
        crate::services::llm_gateway::compute_cost_cents(response.model.as_str(), usage);
    append_session_event(
        appender,
        ctx,
        session_id,
        Event::BrainResponse {
            text: response.text.clone(),
            thought_signature: response.thought_signature.clone(),
            model: response.model.clone(),
            model_tier: ModelTier::Main,
            input_tokens_uncached: usage.input_tokens_uncached,
            input_tokens_cache_write: usage.input_tokens_cache_write,
            input_tokens_cache_read: usage.input_tokens_cache_read,
            output_tokens: usage.output_tokens,
            cost_cents,
            duration_ms: response.duration_ms,
            llm_ttft_ms: None,
        },
    )
    .await
}

/// Records the event sequence used as the next replay cutoff.
pub(super) fn record_last_response_sequence(ctx: &WorkflowContext<'_>, sequence_num: u64) {
    ctx.set(
        driver_progress::RootTurnStateKey::LAST_RESPONSE_SEQUENCE,
        Json::from(sequence_num),
    );
}

/// Returns the first sequence after the last persisted response.
pub(super) async fn last_response_cutoff_before_seq(
    ctx: &WorkflowContext<'_>,
) -> Result<Option<u64>, HandlerError> {
    Ok(ctx
        .get::<Json<u64>>(driver_progress::RootTurnStateKey::LAST_RESPONSE_SEQUENCE)
        .await?
        .map(Json::into_inner)
        .map(|sequence_num| sequence_num.saturating_add(1)))
}

/// Enqueues ingestion of the exact finalized turn paired with a response.
pub(super) async fn ingest_deferred_session_turn(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    request: &CompletionRequest,
    response_sequence_num: u64,
) -> Result<(), HandlerError> {
    let finalized_at = durable_utc_now(ctx, "workflow_utc_now").await?;
    if let Some(turn) = crate::services::llm_gateway::session_turn_from_completion_request(
        request,
        session_id,
        response_sequence_num,
        finalized_at,
    ) {
        // Detached by design: the ingestion object key is derived from the immutable
        // finalized turn, and IngestionVO deduplicates a replay of the same generation.
        crate::restate_identity::replay_safe_request(
            ctx.object_client::<IngestionVOClient>(ingestion_object_key(&turn))
                .ingest_turn(Json(turn)),
        )
        .send();
    }
    Ok(())
}

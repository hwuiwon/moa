//! Exact session-event queries used by root turn assessment and lineage.

use moa_brain::turn_segments::{
    SegmentBoundarySequences, latest_user_message, segment_assessment_to_seq,
};
use moa_core::wire::session_store::GetSegmentBaselineRequest;
use moa_core::{
    events::EventType, traits::SessionStore as _, types::events_stream::EventRange,
    types::events_stream::EventRecord, types::identifiers::SegmentId,
    types::identifiers::SessionId, types::session::SessionMeta,
};
use moa_session::PostgresSessionStore;
use restate_sdk::prelude::*;

use crate::services::session_store::RestateSessionStoreClient;
use crate::turn_driver::progress as driver_progress;

pub(super) async fn load_session_meta(
    ctx: &WorkflowContext<'_>,
    store: Arc<PostgresSessionStore>,
    session_id: SessionId,
) -> Result<SessionMeta, HandlerError> {
    Ok(ctx
        .run(|| async move {
            store
                .get_session(session_id)
                .await
                .map(Json::from)
                .map_err(HandlerError::from)
        })
        .name("turn_execution_load_session_meta")
        .await?
        .into_inner())
}

async fn load_events_in_range(
    ctx: &WorkflowContext<'_>,
    store: Arc<PostgresSessionStore>,
    session_id: SessionId,
    range: EventRange,
    operation_name: &'static str,
) -> Result<Vec<EventRecord>, HandlerError> {
    Ok(ctx
        .run(move || {
            let store = store.clone();
            let range = range.clone();
            async move {
                store
                    .get_events(session_id, range)
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            }
        })
        .name(operation_name)
        .await?
        .into_inner())
}

pub(super) async fn latest_event_cutoff_before_seq(
    ctx: &WorkflowContext<'_>,
    store: Arc<PostgresSessionStore>,
    session_id: SessionId,
) -> Result<Option<u64>, HandlerError> {
    let events = load_events_in_range(
        ctx,
        store,
        session_id,
        EventRange::recent(1),
        "turn_execution_load_latest_event_for_assessment_cutoff",
    )
    .await?;
    Ok(events
        .into_iter()
        .map(|record| record.sequence_num.saturating_add(1))
        .max())
}

pub(super) async fn load_recent_target_events(
    ctx: &WorkflowContext<'_>,
    store: Arc<PostgresSessionStore>,
    session_id: SessionId,
) -> Result<Vec<EventRecord>, HandlerError> {
    load_events_in_range(
        ctx,
        store,
        session_id,
        EventRange {
            event_types: Some(vec![
                EventType::SegmentStarted,
                EventType::SegmentCompleted,
                EventType::UserMessage,
                EventType::BrainResponse,
                EventType::ToolCall,
                EventType::ToolResult,
                EventType::ToolError,
                EventType::WorkerSpawned,
                EventType::WorkerMessageSent,
                EventType::MemoryRead,
                EventType::MemoryWrite,
                EventType::MemoryIngest,
            ]),
            ..EventRange::recent(24)
        },
        "turn_execution_load_recent_target_events",
    )
    .await
}

pub(super) async fn load_segment_boundary_events(
    ctx: &WorkflowContext<'_>,
    store: Arc<PostgresSessionStore>,
    session_id: SessionId,
) -> Result<Vec<EventRecord>, HandlerError> {
    load_events_in_range(
        ctx,
        store,
        session_id,
        EventRange {
            event_types: Some(vec![EventType::SegmentStarted, EventType::SegmentCompleted]),
            ..EventRange::default()
        },
        "turn_execution_load_segment_boundaries",
    )
    .await
}

pub(super) async fn load_segment_assessment_events(
    ctx: &WorkflowContext<'_>,
    store: Arc<PostgresSessionStore>,
    session_id: SessionId,
    segment_id: SegmentId,
    boundary: SegmentBoundarySequences,
    cutoff_before_seq: Option<u64>,
    stop_at_completion: bool,
) -> Result<Vec<EventRecord>, HandlerError> {
    let to_seq = segment_assessment_to_seq(boundary, cutoff_before_seq, stop_at_completion);
    tracing::debug!(
        session_id = %session_id,
        segment_id = %segment_id,
        from_seq = boundary.start_seq,
        to_seq = ?to_seq,
        "loading bounded events for segment assessment"
    );
    load_events_in_range(
        ctx,
        store,
        session_id,
        EventRange {
            from_seq: Some(boundary.start_seq),
            to_seq,
            ..EventRange::default()
        },
        "turn_execution_load_segment_assessment_events",
    )
    .await
}

pub(super) async fn load_next_user_message_cutoff(
    ctx: &WorkflowContext<'_>,
    store: Arc<PostgresSessionStore>,
    session_id: SessionId,
    segment_start_seq: u64,
) -> Result<Option<(String, u64)>, HandlerError> {
    let current_user_sequence = ctx
        .get::<Json<u64>>(driver_progress::RootTurnStateKey::USER_MESSAGE_SEQUENCE)
        .await?
        .map(Json::into_inner)
        .filter(|sequence_num| *sequence_num > segment_start_seq);
    if let Some(sequence_num) = current_user_sequence {
        let events = load_events_in_range(
            ctx,
            store.clone(),
            session_id,
            EventRange {
                from_seq: Some(sequence_num),
                to_seq: Some(sequence_num),
                event_types: Some(vec![EventType::UserMessage]),
                ..EventRange::default()
            },
            "turn_execution_load_current_user_message",
        )
        .await?;
        if let Some((text, sequence_num)) = latest_user_message(&events) {
            return Ok(Some((text.to_string(), sequence_num)));
        }
        tracing::warn!(
            session_id = %session_id,
            sequence_num,
            "current user message sequence was not found during completed segment assessment"
        );
    }

    let events = load_events_in_range(
        ctx,
        store,
        session_id,
        EventRange {
            from_seq: Some(segment_start_seq.saturating_add(1)),
            event_types: Some(vec![EventType::UserMessage]),
            ..EventRange::default()
        },
        "turn_execution_load_segment_user_messages",
    )
    .await?;
    Ok(latest_user_message(&events).map(|(text, sequence_num)| (text.to_string(), sequence_num)))
}

pub(super) async fn load_session_events_fallback(
    ctx: &WorkflowContext<'_>,
    store: Arc<PostgresSessionStore>,
    session_id: SessionId,
    cutoff_before_seq: Option<u64>,
) -> Result<Vec<EventRecord>, HandlerError> {
    let range = EventRange {
        to_seq: cutoff_before_seq.map(|sequence_num| sequence_num.saturating_sub(1)),
        ..EventRange::all()
    };
    Ok(ctx
        .run(move || {
            let store = store.clone();
            let range = range.clone();
            async move {
                store
                    .get_events(session_id, range)
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            }
        })
        .name("turn_execution_load_session_events_fallback")
        .await?
        .into_inner())
}

pub(super) async fn load_segment_baseline(
    ctx: &WorkflowContext<'_>,
    tenant_id: moa_core::types::identifiers::TenantId,
) -> Result<Option<moa_core::types::segment_assessment::SegmentBaseline>, HandlerError> {
    Ok(ctx
        .service_client::<RestateSessionStoreClient>()
        .get_segment_baseline(Json(GetSegmentBaselineRequest { tenant_id }))
        .call()
        .await?
        .into_inner())
}

use std::sync::Arc;

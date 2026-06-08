//! Task segment transition and classification helpers for session turns.

use super::*;

pub(super) async fn ensure_current_segment(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    request: &mut CompletionRequest,
) -> Result<(), HandlerError> {
    let mut state = SessionVoState::load_from(ctx).await?;
    let meta = state
        .meta
        .clone()
        .ok_or_else(|| TerminalError::new("session meta missing"))?;

    if state.current_segment.is_none()
        && let Some(segment) = ctx
            .service_client::<RestateSessionStoreClient>()
            .get_active_segment(Json(session_id))
            .call()
            .await?
            .into_inner()
    {
        state.current_segment = Some(segment.active_view());
    }

    if let Some(transition) = SegmentTracker::transition_from_metadata(
        &request.metadata,
        session_id,
        meta.workspace_id.as_str(),
        &state.current_segment,
        Utc::now(),
    ) {
        if let Some(completed) = transition.completed.clone() {
            ctx.service_client::<RestateSessionStoreClient>()
                .complete_segment(Json(CompleteSegmentRequest {
                    segment_id: completed.segment_id,
                    update: completed.update.clone(),
                }))
                .send();
            ctx.service_client::<RestateSessionStoreClient>()
                .append_event(Json(AppendEventRequest {
                    session_id,
                    event: completed.clone().into_event(),
                }))
                .send();
            score_completed_segment_at_transition(
                ctx,
                session_id,
                meta.workspace_id.as_str(),
                &completed,
                &request.metadata,
            )
            .await?;
        }

        ctx.service_client::<RestateSessionStoreClient>()
            .create_segment(Json(CreateSegmentRequest {
                segment: transition.task_segment.clone(),
            }))
            .send();
        ctx.service_client::<RestateSessionStoreClient>()
            .append_event(Json(AppendEventRequest {
                session_id,
                event: transition.started.clone().into_event(),
            }))
            .send();

        state.set_current_segment(transition.active_segment);
        state.persist_into(ctx);
    }

    if let Some(segment) = state.current_segment.as_ref() {
        request.metadata.insert(
            "_moa.segment_id".to_string(),
            serde_json::json!(segment.id.to_string()),
        );
        request.metadata.insert(
            "_moa.segment_index".to_string(),
            serde_json::json!(segment.segment_index),
        );
    }

    Ok(())
}

//! Persistence helpers for session events and status sync.

use super::*;

pub(super) async fn sync_status(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    state: &SessionVoState,
) -> Result<(), HandlerError> {
    let persist_span = event_persist_span(0);
    let persist_started = Instant::now();
    crate::restate_identity::replay_safe_request(
        ctx.service_client::<RestateSessionStoreClient>()
            .update_status(Json(UpdateStatusRequest {
                session_id,
                status: state.current_status(),
            })),
    )
    .call()
    .instrument(persist_span)
    .await?;
    record_turn_event_persist_duration(persist_started.elapsed(), 0);
    Ok(())
}

pub(super) fn parse_session_key(key: &str) -> Result<SessionId, HandlerError> {
    uuid::Uuid::parse_str(key)
        .map(SessionId)
        .map_err(|error| TerminalError::new(format!("invalid session key `{key}`: {error}")).into())
}

//! Parent session persistence helpers for worker events.

use super::*;

pub(super) async fn persist_parent_session_event(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    event: Event,
) -> Result<(), HandlerError> {
    ctx.service_client::<RestateSessionStoreClient>()
        .append_event(Json(AppendEventRequest {
            session_id,
            event,
            dedupe_key: None,
        }))
        .call()
        .await?;
    Ok(())
}

pub(super) fn render_user_message(message: &UserMessage) -> String {
    moa_core::types::channel::render_user_message_with_attachments(
        &message.text,
        &message.attachments,
    )
}

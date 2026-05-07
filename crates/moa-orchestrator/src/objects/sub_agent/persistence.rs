//! Parent session persistence helpers for sub-agent events.

use super::*;

pub(super) async fn persist_parent_session_event(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    event: Event,
) -> Result<(), HandlerError> {
    ctx.service_client::<RestateSessionStoreClient>()
        .append_event(Json(AppendEventRequest { session_id, event }))
        .call()
        .await?;
    Ok(())
}

pub(super) fn render_user_message(message: &UserMessage) -> String {
    if message.attachments.is_empty() {
        return message.text.clone();
    }

    let attachments = message
        .attachments
        .iter()
        .map(|attachment| attachment.name.clone())
        .collect::<Vec<_>>()
        .join(", ");
    format!("{}\n\nAttachments: {attachments}", message.text)
}

pub(super) fn to_handler_error(error: MoaError) -> HandlerError {
    if error.is_fatal() {
        return TerminalError::new(error.to_string()).into();
    }

    HandlerError::from(error)
}

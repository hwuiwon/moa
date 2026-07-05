//! Live channel delivery for transient turn progress updates.

use moa_core::wire::turn::TurnPhase;
use moa_core::{
    Channel, MessageContent, MessageId, OutboundMessage, SessionChannelBinding,
    SessionChannelBindingId, SessionId, SessionStatus, traits::ChannelAdapter,
};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::ctx::OrchestratorCtx;

const K_PROGRESS_LIVE_DELIVERY_ENABLED: &str = "progress_live_delivery_enabled";
const K_PROGRESS_STATUS_MESSAGE: &str = "progress_status_message";
const MAX_STATUS_SUMMARY_CHARS: usize = 240;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ProgressStatusMessage {
    binding_id: SessionChannelBindingId,
    message_id: MessageId,
}

/// Channel-specific progress delivery behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProgressDeliveryMode {
    /// Deliver `StatusUpdate` messages through a channel adapter.
    LiveStatus,
    /// Do not send channel messages; clients recover progress by polling/replaying events.
    EventReplay,
    /// Do not surface progress mid-turn on this channel.
    Silent,
}

/// Enables live channel delivery for the current workflow.
pub(crate) fn enable_live_delivery(ctx: &WorkflowContext<'_>) {
    ctx.set(K_PROGRESS_LIVE_DELIVERY_ENABLED, Json::from(true));
}

/// Attempts to deliver a user-visible status update for transient turn progress.
pub(crate) async fn maybe_deliver(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    summary: &str,
) -> Result<(), HandlerError> {
    if let Err(error) = try_deliver_status(ctx, session_id, SessionStatus::Running, summary).await {
        tracing::warn!(
            session_id = %session_id,
            error = ?error,
            "progress status delivery skipped"
        );
    }
    Ok(())
}

/// Attempts to deliver a terminal status update for a completed root turn.
pub(crate) async fn maybe_deliver_terminal(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    phase: TurnPhase,
) -> Result<(), HandlerError> {
    let (status, summary) = terminal_status_and_summary(phase);
    if let Err(error) = try_deliver_status(ctx, session_id, status, summary).await {
        tracing::warn!(
            session_id = %session_id,
            error = ?error,
            "terminal progress status delivery skipped"
        );
    }
    Ok(())
}

async fn try_deliver_status(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    status: SessionStatus,
    summary: &str,
) -> Result<(), HandlerError> {
    if !live_delivery_enabled(ctx).await? {
        return Ok(());
    }

    let Some(binding) = load_active_channel_binding(ctx, session_id).await? else {
        return Ok(());
    };
    if progress_delivery_mode(binding.channel_ref.channel()) != ProgressDeliveryMode::LiveStatus {
        return Ok(());
    }

    let Some(adapter) = OrchestratorCtx::current_channel_adapter(binding.channel_ref.channel())
    else {
        return Ok(());
    };
    let existing = ctx
        .get::<Json<ProgressStatusMessage>>(K_PROGRESS_STATUS_MESSAGE)
        .await?
        .map(Json::into_inner);
    let existing_message_id = existing_message_for_binding(existing.as_ref(), binding.binding_id);
    let message = status_update_message(session_id, status, summary, binding.clone());
    let supports_edit = adapter.capabilities().supports_edit;
    let next_message_id =
        deliver_status_message(ctx, adapter, existing_message_id, supports_edit, message).await?;
    if let Some(message_id) = next_message_id {
        ctx.set(
            K_PROGRESS_STATUS_MESSAGE,
            Json::from(ProgressStatusMessage {
                binding_id: binding.binding_id,
                message_id,
            }),
        );
    }
    Ok(())
}

fn terminal_status_and_summary(phase: TurnPhase) -> (SessionStatus, &'static str) {
    match phase {
        TurnPhase::Completed => (SessionStatus::Completed, "Completed."),
        TurnPhase::Cancelled => (SessionStatus::Cancelled, "Cancelled."),
        TurnPhase::Failed => (SessionStatus::Failed, "Failed."),
        _ => (SessionStatus::Running, "Working on it."),
    }
}

fn existing_message_for_binding(
    existing: Option<&ProgressStatusMessage>,
    binding_id: SessionChannelBindingId,
) -> Option<MessageId> {
    existing
        .filter(|status| status.binding_id == binding_id)
        .map(|status| status.message_id.clone())
}

fn progress_delivery_mode(channel: Channel) -> ProgressDeliveryMode {
    match channel {
        Channel::Slack => ProgressDeliveryMode::LiveStatus,
        Channel::Chat => ProgressDeliveryMode::EventReplay,
        Channel::Email | Channel::Sms => ProgressDeliveryMode::Silent,
    }
}

fn status_update_message(
    session_id: SessionId,
    status: SessionStatus,
    summary: &str,
    binding: SessionChannelBinding,
) -> OutboundMessage {
    OutboundMessage {
        content: MessageContent::StatusUpdate {
            session_id,
            status,
            summary: compact_status_summary(summary),
        },
        channel_ref: Some(binding.channel_ref),
        reply_to: None,
        ephemeral: false,
    }
}

fn compact_status_summary(summary: &str) -> String {
    let mut compact = summary
        .trim()
        .chars()
        .take(MAX_STATUS_SUMMARY_CHARS)
        .collect::<String>();
    if summary.trim().chars().count() > MAX_STATUS_SUMMARY_CHARS {
        compact.push_str("...");
    }
    compact
}

async fn live_delivery_enabled(ctx: &WorkflowContext<'_>) -> Result<bool, HandlerError> {
    Ok(ctx
        .get::<Json<bool>>(K_PROGRESS_LIVE_DELIVERY_ENABLED)
        .await?
        .map(Json::into_inner)
        .unwrap_or(false))
}

async fn load_active_channel_binding(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
) -> Result<Option<SessionChannelBinding>, HandlerError> {
    let store = OrchestratorCtx::current_session_store();
    Ok(ctx
        .run(move || {
            let store = store.clone();
            async move {
                store
                    .get_active_session_channel_binding(session_id)
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            }
        })
        .name("turn_progress_load_active_channel_binding")
        .await?
        .into_inner())
}

async fn deliver_status_message(
    ctx: &WorkflowContext<'_>,
    adapter: Arc<dyn ChannelAdapter>,
    existing_message_id: Option<MessageId>,
    supports_edit: bool,
    message: OutboundMessage,
) -> Result<Option<MessageId>, HandlerError> {
    Ok(ctx
        .run(|| async move {
            let next_id =
                send_or_edit_status_message(adapter, existing_message_id, supports_edit, message)
                    .await;
            Ok::<_, HandlerError>(Json::from(next_id))
        })
        .name("turn_progress_deliver_status_update")
        .await?
        .into_inner())
}

async fn send_or_edit_status_message(
    adapter: Arc<dyn ChannelAdapter>,
    existing_message_id: Option<MessageId>,
    supports_edit: bool,
    message: OutboundMessage,
) -> Option<MessageId> {
    if let Some(existing) = existing_message_id.as_ref()
        && supports_edit
    {
        match adapter.edit(existing, message.clone()).await {
            Ok(()) => return Some(existing.clone()),
            Err(error) => {
                tracing::warn!(
                    message_id = %existing,
                    error = %error,
                    "progress status edit failed; sending a replacement status message"
                );
            }
        }
    } else if existing_message_id.is_some() {
        return existing_message_id;
    }

    match adapter.send(message).await {
        Ok(message_id) => Some(message_id),
        Err(error) => {
            tracing::warn!(
                error = %error,
                "progress status delivery failed"
            );
            existing_message_id
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use moa_core::{
        ChannelCapabilities, ChannelEvent, ChannelRef, MoaError, Result as MoaResult,
        SessionChannelBindingId,
    };
    use tokio::sync::mpsc;

    use super::*;

    struct FakeChannelAdapter {
        supports_edit: bool,
        edit_fails: bool,
        send_fails: bool,
        send_id: MessageId,
        edit_calls: AtomicUsize,
        send_calls: AtomicUsize,
    }

    impl FakeChannelAdapter {
        fn new(supports_edit: bool) -> Self {
            Self {
                supports_edit,
                edit_fails: false,
                send_fails: false,
                send_id: MessageId::new("sent-status"),
                edit_calls: AtomicUsize::new(0),
                send_calls: AtomicUsize::new(0),
            }
        }

        fn with_edit_failure(mut self) -> Self {
            self.edit_fails = true;
            self
        }

        fn with_send_failure(mut self) -> Self {
            self.send_fails = true;
            self
        }

        fn edit_calls(&self) -> usize {
            self.edit_calls.load(Ordering::SeqCst)
        }

        fn send_calls(&self) -> usize {
            self.send_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ChannelAdapter for FakeChannelAdapter {
        fn channel(&self) -> Channel {
            Channel::Slack
        }

        fn capabilities(&self) -> ChannelCapabilities {
            ChannelCapabilities {
                max_message_length: 40_000,
                supports_ephemeral: false,
                supports_threads: true,
                supports_code_blocks: true,
                supports_edit: self.supports_edit,
                supports_reactions: false,
                min_edit_interval: std::time::Duration::ZERO,
            }
        }

        async fn start(&self, _event_tx: mpsc::Sender<ChannelEvent>) -> MoaResult<()> {
            Ok(())
        }

        async fn send(&self, _msg: OutboundMessage) -> MoaResult<MessageId> {
            self.send_calls.fetch_add(1, Ordering::SeqCst);
            if self.send_fails {
                return Err(MoaError::ProviderError("send failed".to_string()));
            }
            Ok(self.send_id.clone())
        }

        async fn edit(&self, _msg_id: &MessageId, _msg: OutboundMessage) -> MoaResult<()> {
            self.edit_calls.fetch_add(1, Ordering::SeqCst);
            if self.edit_fails {
                return Err(MoaError::ProviderError("edit failed".to_string()));
            }
            Ok(())
        }

        async fn delete(&self, _msg_id: &MessageId) -> MoaResult<()> {
            Ok(())
        }
    }

    fn status_message() -> OutboundMessage {
        status_update_message(
            SessionId::new(),
            SessionStatus::Running,
            "Calling the model",
            SessionChannelBinding {
                binding_id: SessionChannelBindingId::new(),
                channel_ref: ChannelRef::Slack {
                    team_id: Some("T123".to_string()),
                    slack_channel_id: Some("C123".to_string()),
                    thread_ts: Some("1712668800.000100".to_string()),
                    user_id: Some("U123".to_string()),
                },
            },
        )
    }

    #[test]
    fn progress_delivery_mode_is_channel_aware() {
        // Pins: live chat channels can surface progress, async channels stay quiet.
        assert_eq!(
            progress_delivery_mode(Channel::Slack),
            ProgressDeliveryMode::LiveStatus
        );
        assert_eq!(
            progress_delivery_mode(Channel::Chat),
            ProgressDeliveryMode::EventReplay
        );
        assert_eq!(
            progress_delivery_mode(Channel::Email),
            ProgressDeliveryMode::Silent
        );
        assert_eq!(
            progress_delivery_mode(Channel::Sms),
            ProgressDeliveryMode::Silent
        );
    }

    #[test]
    fn status_update_message_uses_resolved_route_without_reply_anchor() {
        // Pins: workflow-originated progress is routed by durable binding, not process-local Slack state.
        let session_id = SessionId::new();
        let channel_ref = ChannelRef::Slack {
            team_id: Some("T123".to_string()),
            slack_channel_id: Some("C123".to_string()),
            thread_ts: Some("1712668800.000100".to_string()),
            user_id: Some("U123".to_string()),
        };
        let message = status_update_message(
            session_id,
            SessionStatus::Running,
            "Calling the model",
            SessionChannelBinding {
                binding_id: SessionChannelBindingId::new(),
                channel_ref: channel_ref.clone(),
            },
        );

        assert_eq!(message.channel_ref, Some(channel_ref));
        assert_eq!(message.reply_to, None);
        assert!(!message.ephemeral);
        assert_eq!(
            message.content,
            MessageContent::StatusUpdate {
                session_id,
                status: SessionStatus::Running,
                summary: "Calling the model".to_string(),
            }
        );
    }

    #[test]
    fn stored_status_message_only_edits_matching_binding() {
        // Pins: channel switches send a fresh progress status instead of editing the old route.
        let first_binding = SessionChannelBindingId::new();
        let second_binding = SessionChannelBindingId::new();
        let message_id = MessageId::new("slack:C123:1712668800.000100");
        let stored = ProgressStatusMessage {
            binding_id: first_binding,
            message_id: message_id.clone(),
        };

        assert_eq!(
            existing_message_for_binding(Some(&stored), first_binding),
            Some(message_id)
        );
        assert_eq!(
            existing_message_for_binding(Some(&stored), second_binding),
            None
        );
    }

    #[test]
    fn terminal_status_update_uses_terminal_session_status() {
        // Pins: the edited Slack status line does not remain in a running state after the turn ends.
        let (status, summary) = terminal_status_and_summary(TurnPhase::Completed);
        let session_id = SessionId::new();
        let message = status_update_message(
            session_id,
            status,
            summary,
            SessionChannelBinding {
                binding_id: SessionChannelBindingId::new(),
                channel_ref: ChannelRef::Slack {
                    team_id: Some("T123".to_string()),
                    slack_channel_id: Some("C123".to_string()),
                    thread_ts: Some("1712668800.000100".to_string()),
                    user_id: Some("U123".to_string()),
                },
            },
        );

        assert_eq!(
            message.content,
            MessageContent::StatusUpdate {
                session_id,
                status: SessionStatus::Completed,
                summary: "Completed.".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn status_delivery_edits_existing_message_when_supported() {
        // Pins: Slack progress updates edit the existing status line instead of posting duplicates.
        let adapter = Arc::new(FakeChannelAdapter::new(true));
        let existing = MessageId::new("slack:C123:1712668800.000100");

        let next = send_or_edit_status_message(
            adapter.clone(),
            Some(existing.clone()),
            true,
            status_message(),
        )
        .await;

        assert_eq!(next, Some(existing));
        assert_eq!(adapter.edit_calls(), 1);
        assert_eq!(adapter.send_calls(), 0);
    }

    #[tokio::test]
    async fn status_delivery_sends_replacement_when_edit_fails() {
        // Pins: a stale or restart-missing Slack edit falls back to one replacement status message.
        let adapter = Arc::new(FakeChannelAdapter::new(true).with_edit_failure());

        let next = send_or_edit_status_message(
            adapter.clone(),
            Some(MessageId::new("slack:C123:1712668800.000100")),
            true,
            status_message(),
        )
        .await;

        assert_eq!(next, Some(MessageId::new("sent-status")));
        assert_eq!(adapter.edit_calls(), 1);
        assert_eq!(adapter.send_calls(), 1);
    }

    #[tokio::test]
    async fn status_delivery_send_failure_is_non_fatal_without_existing_message() {
        // Pins: live progress delivery can fail without failing the durable turn after the event is recorded.
        let adapter = Arc::new(FakeChannelAdapter::new(true).with_send_failure());

        let next = send_or_edit_status_message(adapter.clone(), None, true, status_message()).await;

        assert_eq!(next, None);
        assert_eq!(adapter.edit_calls(), 0);
        assert_eq!(adapter.send_calls(), 1);
    }
}

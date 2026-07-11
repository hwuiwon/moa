//! Slack Socket Mode inbound event normalization.

use std::sync::Arc;

use chrono::Utc;
use moa_core::{
    error::MoaError, error::Result, types::channel::Channel, types::channel::ChannelActor,
    types::channel::ChannelEvent, types::channel::ChannelRef,
    types::channel::ChannelSessionCommand, types::channel::InboundMessage,
};
use moka::future::Cache;
use slack_morphism::prelude::*;
use tokio::sync::mpsc;
use tracing::{Instrument, warn};

use crate::messaging_receive_span;

use super::refs::SlackMessageRef;

#[derive(Clone)]
pub(super) struct SlackListenerState {
    pub(super) event_tx: mpsc::Sender<ChannelEvent>,
    pub(super) inbound_contexts: Cache<String, SlackMessageRef>,
}

/// Normalizes one Slack Events API callback JSON payload into MOA's canonical channel event shape.
pub fn normalize_event_json(payload: &str) -> Result<ChannelEvent> {
    let event: SlackPushEventCallback = serde_json::from_str(payload)?;
    normalize_push_event(&event)
}

/// Normalizes one parsed Slack push event into MOA's canonical channel event shape.
pub fn normalize_push_event(event: &SlackPushEventCallback) -> Result<ChannelEvent> {
    inbound_from_push_event(event)
        .ok_or_else(|| {
            MoaError::ValidationError("slack event is not a supported user message".to_string())
        })
        .map(channel_event_from_inbound)
}

pub(super) async fn handle_push_event(
    event: SlackPushEventCallback,
    _client: Arc<SlackClient<SlackClientHyperHttpsConnector>>,
    state: SlackClientEventsUserState,
) -> UserCallbackResult<()> {
    let shared = {
        let guard = state.read().await;
        guard.get_user_state::<SlackListenerState>().cloned()
    };
    let Some(shared) = shared else {
        warn!("slack listener state missing for push event");
        return Ok(());
    };

    if let Some(inbound) = inbound_from_push_event(&event) {
        let channel_event = channel_event_from_inbound(inbound);
        let inbound = inbound_for_event(&channel_event);
        let channel_msg_id = inbound.channel_msg_id.clone();
        let messaging_span = messaging_receive_span(inbound);
        async move {
            if let Some(origin) = push_event_origin(&event) {
                shared.inbound_contexts.insert(channel_msg_id, origin).await;
            }
            if shared.event_tx.send(channel_event).await.is_err() {
                warn!("slack inbound receiver dropped");
            }
        }
        .instrument(messaging_span)
        .await;
    }
    Ok(())
}

pub(super) fn inbound_from_push_event(event: &SlackPushEventCallback) -> Option<InboundMessage> {
    let team_id = Some(event.team_id.as_ref());
    match &event.event {
        SlackEventCallbackBody::AppMention(message) => inbound_from_app_mention(message, team_id),
        SlackEventCallbackBody::Message(message) => inbound_from_message_event(message, team_id),
        _ => None,
    }
}

fn channel_event_from_inbound(inbound: InboundMessage) -> ChannelEvent {
    match parse_session_command(&inbound.text) {
        Some(SlackSessionCommand::Status) => {
            ChannelEvent::SessionCommand(ChannelSessionCommand::Status(inbound))
        }
        Some(SlackSessionCommand::Stop) => {
            ChannelEvent::SessionCommand(ChannelSessionCommand::Stop(inbound))
        }
        None => ChannelEvent::Message(inbound),
    }
}

fn inbound_for_event(event: &ChannelEvent) -> &InboundMessage {
    match event {
        ChannelEvent::Message(inbound) => inbound,
        ChannelEvent::SessionCommand(ChannelSessionCommand::Status(inbound))
        | ChannelEvent::SessionCommand(ChannelSessionCommand::Stop(inbound)) => inbound,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlackSessionCommand {
    Status,
    Stop,
}

fn parse_session_command(text: &str) -> Option<SlackSessionCommand> {
    match text.trim() {
        "/moa status" => Some(SlackSessionCommand::Status),
        "/moa stop" => Some(SlackSessionCommand::Stop),
        _ => None,
    }
}

fn push_event_origin(event: &SlackPushEventCallback) -> Option<SlackMessageRef> {
    match &event.event {
        SlackEventCallbackBody::AppMention(message) => Some(SlackMessageRef {
            channel_id: Arc::<str>::from(message.channel.0.clone()),
            ts: message.origin.ts.0.clone(),
            thread_ts: message.origin.thread_ts.as_ref().map(|ts| ts.0.clone()),
        }),
        SlackEventCallbackBody::Message(message) => Some(SlackMessageRef {
            channel_id: Arc::<str>::from(message.origin.channel.as_ref()?.0.clone()),
            ts: message.origin.ts.0.clone(),
            thread_ts: message.origin.thread_ts.as_ref().map(|ts| ts.0.clone()),
        }),
        _ => None,
    }
}

fn inbound_from_app_mention(
    message: &SlackAppMentionEvent,
    team_id: Option<&str>,
) -> Option<InboundMessage> {
    let text = message.content.text.clone()?;
    let channel_msg_id = message.origin.ts.0.clone();
    let user_id = message.user.0.clone();
    Some(InboundMessage {
        channel: Channel::Slack,
        channel_msg_id,
        actor: ChannelActor {
            external_id: user_id.clone(),
            display_name: format!("<@{}>", message.user.0),
            channel_account_id: None,
            moa_user_id: None,
        },
        channel_ref: slack_channel_ref(
            team_id,
            &message.channel.0,
            message.origin.thread_ts.as_ref().map(|ts| ts.0.as_str()),
            &user_id,
        ),
        text,
        attachments: Vec::new(),
        reply_to: message.origin.thread_ts.as_ref().map(|ts| ts.0.clone()),
        timestamp: slack_ts_to_datetime(&message.origin.ts.0),
    })
}

fn inbound_from_message_event(
    message: &SlackMessageEvent,
    team_id: Option<&str>,
) -> Option<InboundMessage> {
    if message.subtype.is_some() {
        return None;
    }

    let text = message.content.as_ref()?.text.clone()?;
    let user_id = message.sender.user.as_ref()?.0.clone();
    let channel_id = message.origin.channel.as_ref()?.0.clone();

    Some(InboundMessage {
        channel: Channel::Slack,
        channel_msg_id: message.origin.ts.0.clone(),
        actor: ChannelActor {
            external_id: user_id.clone(),
            display_name: slack_sender_name(&message.sender),
            channel_account_id: None,
            moa_user_id: None,
        },
        channel_ref: slack_channel_ref(
            team_id,
            &channel_id,
            message.origin.thread_ts.as_ref().map(|ts| ts.0.as_str()),
            &user_id,
        ),
        text,
        attachments: Vec::new(),
        reply_to: message.origin.thread_ts.as_ref().map(|ts| ts.0.clone()),
        timestamp: slack_ts_to_datetime(&message.origin.ts.0),
    })
}

fn slack_channel_ref(
    team_id: Option<&str>,
    channel_id: &str,
    thread_ts: Option<&str>,
    user_id: &str,
) -> ChannelRef {
    ChannelRef::Slack {
        team_id: team_id.map(ToOwned::to_owned),
        slack_channel_id: Some(channel_id.to_string()),
        // Direct-message channels never thread their replies; channel posts carry
        // the originating thread when one is present.
        thread_ts: if channel_id.starts_with('D') {
            None
        } else {
            thread_ts.map(ToOwned::to_owned)
        },
        user_id: Some(user_id.to_string()),
    }
}

fn slack_sender_name(sender: &SlackMessageSender) -> String {
    sender
        .username
        .clone()
        .or_else(|| sender.user.as_ref().map(|user| format!("<@{}>", user.0)))
        .unwrap_or_else(|| "Slack User".to_string())
}

fn slack_ts_to_datetime(value: &str) -> chrono::DateTime<Utc> {
    let seconds = value
        .split('.')
        .next()
        .and_then(|seconds| seconds.parse::<i64>().ok())
        .unwrap_or(0);
    chrono::DateTime::<Utc>::from_timestamp(seconds, 0).unwrap_or_else(Utc::now)
}

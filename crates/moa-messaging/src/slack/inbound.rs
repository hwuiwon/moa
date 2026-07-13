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

    if let Some(mut inbound) = inbound_from_push_event(&event) {
        let channel_msg_id = inbound.channel_msg_id.clone();
        let messaging_span = messaging_receive_span(&inbound);
        async move {
            // Slack Socket Mode delivers events over a websocket with no per-event
            // HTTP headers, so serialize this receive span's trace context onto the
            // message. The ingress consumer re-adopts it (see `handle_channel_event`)
            // so the downstream operation joins this trace instead of a fresh root.
            carry_trace_context(&mut inbound);
            let channel_event = channel_event_from_inbound(inbound);
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

/// Serializes the current span's W3C trace context onto the message.
///
/// Slack Socket Mode has no per-event request headers, so this is how the
/// inbound-receive span's trace reaches the ingress consumer, which re-adopts it.
/// Leaves `trace_headers` empty when no sampled span context is active.
fn carry_trace_context(inbound: &mut InboundMessage) {
    inbound.trace_headers = moa_observability::current_trace_headers()
        .into_iter()
        .collect();
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
        trace_headers: std::collections::BTreeMap::new(),
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
        trace_headers: std::collections::BTreeMap::new(),
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

#[cfg(test)]
mod tests {
    use moa_core::types::channel::{Channel, ChannelActor, ChannelRef, InboundMessage};
    use opentelemetry::trace::{TraceContextExt, TraceId, TracerProvider as _};
    use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    use tracing_subscriber::layer::SubscriberExt;

    use super::carry_trace_context;

    fn test_inbound() -> InboundMessage {
        InboundMessage {
            channel: Channel::Slack,
            channel_msg_id: "1700000000.000200".to_string(),
            actor: ChannelActor {
                external_id: "U123".to_string(),
                display_name: "<@U123>".to_string(),
                channel_account_id: None,
                moa_user_id: None,
            },
            channel_ref: ChannelRef::Slack {
                team_id: Some("T123".to_string()),
                slack_channel_id: Some("C123".to_string()),
                thread_ts: None,
                user_id: Some("U123".to_string()),
            },
            text: "hello".to_string(),
            attachments: Vec::new(),
            reply_to: None,
            timestamp: chrono::Utc::now(),
            trace_headers: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn slack_receive_trace_context_carries_onto_message_and_readopts_same_trace() {
        // Pins: Slack Socket Mode has no request headers, so the receive span's W3C trace
        // context is serialized onto the InboundMessage (carry) and re-adopted by the
        // consumer, placing the consumer span in the SAME trace — the websocket-path
        // equivalent of header propagation.
        moa_observability::init_trace_propagation();
        let provider = SdkTracerProvider::builder()
            .with_sampler(Sampler::AlwaysOn)
            .build();
        let tracer = provider.tracer("moa-messaging-test");
        let layer = tracing_opentelemetry::layer().with_tracer(tracer);
        let subscriber = tracing_subscriber::registry().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            let mut inbound = test_inbound();

            // Sender side: capture the receive span's context onto the message.
            let receive_span = tracing::info_span!("messaging_receive");
            let receive_trace = receive_span.in_scope(|| {
                carry_trace_context(&mut inbound);
                receive_span.context().span().span_context().trace_id()
            });

            assert!(
                inbound.trace_headers.contains_key("traceparent"),
                "receive span must serialize a traceparent onto the message, got {:?}",
                inbound.trace_headers
            );
            assert_ne!(receive_trace, TraceId::INVALID);

            // Consumer side: a fresh, otherwise-rootless span re-adopts the carried context.
            let consume_span = tracing::info_span!("channel_session_command");
            moa_observability::adopt_remote_parent(&consume_span, |name| {
                inbound.trace_headers.get(name).cloned()
            });
            let consume_trace = consume_span.context().span().span_context().trace_id();

            assert_eq!(
                consume_trace, receive_trace,
                "consumer span must join the receive span's trace"
            );
        });
    }

    #[test]
    fn empty_trace_headers_are_omitted_but_populated_ones_round_trip() {
        // Pins: the carry field is skipped on the wire when empty (zero cost on the common
        // path) and preserves captured headers through serde when present.
        let inbound = test_inbound();
        let json = serde_json::to_value(&inbound).expect("serialize");
        assert!(
            json.get("trace_headers").is_none(),
            "empty trace_headers must be skipped on the wire, got {json}"
        );

        let mut with_headers = test_inbound();
        with_headers
            .trace_headers
            .insert("traceparent".to_string(), "abc".to_string());
        let round: InboundMessage =
            serde_json::from_value(serde_json::to_value(&with_headers).expect("serialize"))
                .expect("deserialize");
        assert_eq!(
            round.trace_headers.get("traceparent").map(String::as_str),
            Some("abc"),
            "populated trace_headers must round-trip through serde"
        );
    }
}

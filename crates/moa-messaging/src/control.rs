//! Slash-command and active-session control-signal translation.

use moa_core::{
    Attachment, Channel, InboundMessage, MessageContent, OutboundMessage, SessionId, SessionSignal,
    UserMessage,
};

/// Messaging interpretation of one inbound channel message.
#[derive(Debug, Clone, PartialEq)]
pub struct MessagingControlAction {
    /// Signal to deliver to the orchestrator session, when this message controls a session.
    pub signal: Option<SessionSignal>,
    /// User-visible acknowledgement rendered by the channel adapter.
    pub acknowledgement: OutboundMessage,
}

/// Translates an inbound message into a session control signal or queue acknowledgement.
pub fn control_action_for_inbound(
    channel: Channel,
    session_id: &SessionId,
    inbound: &InboundMessage,
    session_running: bool,
) -> Option<MessagingControlAction> {
    let text = inbound.text.trim();
    if text.starts_with('/') {
        return Some(command_action(channel, text));
    }

    if session_running {
        return Some(MessagingControlAction {
            signal: Some(SessionSignal::QueueMessage(UserMessage {
                text: inbound.text.clone(),
                attachments: inbound.attachments.clone(),
            })),
            acknowledgement: acknowledgement(
                &channel,
                "Queued - will be picked up after current task",
                false,
            ),
        });
    }

    let _ = session_id;
    None
}

fn command_action(channel: Channel, text: &str) -> MessagingControlAction {
    let mut parts = text.split_whitespace();
    let command = parts.next().unwrap_or_default();
    match command {
        "/stop" => {
            let force = parts.any(|part| matches!(part, "--force" | "force"));
            MessagingControlAction {
                signal: Some(if force {
                    SessionSignal::HardCancel
                } else {
                    SessionSignal::SoftCancel
                }),
                acknowledgement: acknowledgement(
                    &channel,
                    if force {
                        "Stopping immediately..."
                    } else {
                        "Stopping..."
                    },
                    channel == Channel::Slack,
                ),
            }
        }
        "/queue" => MessagingControlAction {
            signal: Some(SessionSignal::QueueMessage(UserMessage {
                text: parts.collect::<Vec<_>>().join(" "),
                attachments: Vec::<Attachment>::new(),
            })),
            acknowledgement: acknowledgement(
                &channel,
                "Queued - will be picked up after current task",
                channel == Channel::Slack,
            ),
        },
        _ => MessagingControlAction {
            signal: None,
            acknowledgement: acknowledgement(
                &channel,
                "Unknown command. Valid commands: /stop, /stop --force, /queue <message>",
                channel == Channel::Slack,
            ),
        },
    }
}

fn acknowledgement(_channel: &Channel, text: &str, ephemeral: bool) -> OutboundMessage {
    OutboundMessage {
        content: MessageContent::Text(text.to_string()),
        buttons: Vec::new(),
        channel_ref: None,
        reply_to: None,
        ephemeral,
    }
}

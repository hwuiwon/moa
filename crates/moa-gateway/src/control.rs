//! Slash-command and active-session control-signal translation.

use moa_core::{
    Attachment, InboundMessage, MessageContent, OutboundMessage, Platform, SessionId,
    SessionSignal, UserMessage,
};

/// Gateway interpretation of one inbound platform message.
#[derive(Debug, Clone, PartialEq)]
pub struct GatewayControlAction {
    /// Signal to deliver to the orchestrator session, when this message controls a session.
    pub signal: Option<SessionSignal>,
    /// User-visible acknowledgement rendered by the platform adapter.
    pub acknowledgement: OutboundMessage,
}

/// Translates an inbound message into a session control signal or queue acknowledgement.
pub fn control_action_for_inbound(
    platform: Platform,
    session_id: &SessionId,
    inbound: &InboundMessage,
    session_running: bool,
) -> Option<GatewayControlAction> {
    let text = inbound.text.trim();
    if text.starts_with('/') {
        return Some(command_action(platform, text));
    }

    if session_running {
        return Some(GatewayControlAction {
            signal: Some(SessionSignal::QueueMessage(UserMessage {
                text: inbound.text.clone(),
                attachments: inbound.attachments.clone(),
            })),
            acknowledgement: acknowledgement(
                &platform,
                "Queued - will be picked up after current task",
                false,
            ),
        });
    }

    let _ = session_id;
    None
}

fn command_action(platform: Platform, text: &str) -> GatewayControlAction {
    let mut parts = text.split_whitespace();
    let command = parts.next().unwrap_or_default();
    match command {
        "/stop" => {
            let force = parts.any(|part| matches!(part, "--force" | "force"));
            GatewayControlAction {
                signal: Some(if force {
                    SessionSignal::HardCancel
                } else {
                    SessionSignal::SoftCancel
                }),
                acknowledgement: acknowledgement(
                    &platform,
                    if force {
                        "Stopping immediately..."
                    } else {
                        "Stopping..."
                    },
                    platform == Platform::Slack,
                ),
            }
        }
        "/queue" => GatewayControlAction {
            signal: Some(SessionSignal::QueueMessage(UserMessage {
                text: parts.collect::<Vec<_>>().join(" "),
                attachments: Vec::<Attachment>::new(),
            })),
            acknowledgement: acknowledgement(
                &platform,
                "Queued - will be picked up after current task",
                platform == Platform::Slack,
            ),
        },
        _ => GatewayControlAction {
            signal: None,
            acknowledgement: acknowledgement(
                &platform,
                "Unknown command. Valid commands: /stop, /stop --force, /queue <message>",
                platform == Platform::Slack,
            ),
        },
    }
}

fn acknowledgement(_platform: &Platform, text: &str, ephemeral: bool) -> OutboundMessage {
    OutboundMessage {
        content: MessageContent::Text(text.to_string()),
        buttons: Vec::new(),
        reply_to: None,
        ephemeral,
    }
}

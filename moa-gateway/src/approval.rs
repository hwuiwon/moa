//! Unified approval rendering and callback handling across platform adapters.

use moa_core::{
    ActionButton, ApprovalRequest, ButtonStyle, MessageContent, OutboundMessage, Platform,
    PlatformCapabilities,
};
use uuid::Uuid;

/// One approval callback action encoded into a platform button payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalCallbackAction {
    /// Allow this request one time.
    AllowOnce {
        /// Approval request identifier.
        request_id: Uuid,
    },
    /// Persist an always-allow rule and approve this request.
    AlwaysAllow {
        /// Approval request identifier.
        request_id: Uuid,
    },
    /// Deny this request.
    Deny {
        /// Approval request identifier.
        request_id: Uuid,
    },
}

impl ApprovalCallbackAction {
    /// Encodes the callback action into a compact cross-platform payload.
    pub fn encode(&self) -> String {
        match self {
            Self::AllowOnce { request_id } => format!("ap:o:{request_id}"),
            Self::AlwaysAllow { request_id } => format!("ap:a:{request_id}"),
            Self::Deny { request_id } => format!("ap:d:{request_id}"),
        }
    }

    /// Decodes a platform callback payload into an approval action.
    pub fn decode(value: &str) -> Option<Self> {
        let mut parts = value.split(':');
        let prefix = parts.next()?;
        let action = parts.next()?;
        let request_id = parts.next()?;
        if prefix != "ap" || parts.next().is_some() {
            return None;
        }

        let request_id = Uuid::parse_str(request_id).ok()?;
        match action {
            "o" => Some(Self::AllowOnce { request_id }),
            "a" => Some(Self::AlwaysAllow { request_id }),
            "d" => Some(Self::Deny { request_id }),
            _ => None,
        }
    }

    /// Converts the callback action into the normalized approval command consumed upstream.
    pub fn inbound_command(&self) -> String {
        match self {
            Self::AllowOnce { request_id } => format!("/approval allow {request_id}"),
            Self::AlwaysAllow { request_id } => format!("/approval always {request_id}"),
            Self::Deny { request_id } => format!("/approval deny {request_id}"),
        }
    }
}

/// Adds platform-native approval affordances to an outbound message when possible.
pub fn prepare_outbound_message(
    platform: Platform,
    capabilities: &PlatformCapabilities,
    mut message: OutboundMessage,
) -> OutboundMessage {
    let request = match &message.content {
        MessageContent::ApprovalRequest { request } => request.clone(),
        _ => return message,
    };

    if message.buttons.is_empty() && capabilities.supports_inline_buttons {
        message.buttons = approval_buttons(platform, request.request_id);
        return message;
    }

    if !capabilities.supports_inline_buttons {
        message.content = MessageContent::Markdown(text_fallback(&request));
    }

    message
}

/// Builds the standard approval buttons for one request and platform.
pub fn approval_buttons(platform: Platform, request_id: Uuid) -> Vec<ActionButton> {
    let (allow_label, always_label, deny_label) = match platform {
        Platform::Slack => ("Allow", "Always", "Deny"),
        _ => ("✅ Allow", "🔁 Always", "❌ Deny"),
    };

    vec![
        ActionButton {
            id: "allow".to_string(),
            label: allow_label.to_string(),
            style: ButtonStyle::Primary,
            callback_data: ApprovalCallbackAction::AllowOnce { request_id }.encode(),
        },
        ActionButton {
            id: "always".to_string(),
            label: always_label.to_string(),
            style: ButtonStyle::Secondary,
            callback_data: ApprovalCallbackAction::AlwaysAllow { request_id }.encode(),
        },
        ActionButton {
            id: "deny".to_string(),
            label: deny_label.to_string(),
            style: ButtonStyle::Danger,
            callback_data: ApprovalCallbackAction::Deny { request_id }.encode(),
        },
    ]
}

fn text_fallback(request: &ApprovalRequest) -> String {
    format!(
        "{} Approval required: {}\n{}\nRequest: {}\n\nReply with one of:\n- /approval allow {}\n- /approval always {}\n- /approval deny {}",
        risk_icon(request),
        request.tool_name,
        request.input_summary,
        request.request_id,
        request.request_id,
        request.request_id,
        request.request_id
    )
}

fn risk_icon(request: &ApprovalRequest) -> &'static str {
    match request.risk_level {
        moa_core::RiskLevel::Low => "🟢",
        moa_core::RiskLevel::Medium => "🟡",
        moa_core::RiskLevel::High => "🔴",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moa_core::RiskLevel;

    fn approval_request() -> ApprovalRequest {
        ApprovalRequest {
            request_id: Uuid::now_v7(),
            tool_name: "bash".to_string(),
            input_summary: "npm test".to_string(),
            risk_level: RiskLevel::High,
        }
    }

    #[test]
    fn callback_data_roundtrips() {
        let request_id = Uuid::now_v7();
        for action in [
            ApprovalCallbackAction::AllowOnce { request_id },
            ApprovalCallbackAction::AlwaysAllow { request_id },
            ApprovalCallbackAction::Deny { request_id },
        ] {
            let encoded = action.encode();
            assert!(encoded.len() <= 64);
            assert_eq!(ApprovalCallbackAction::decode(&encoded), Some(action));
        }
    }

    #[test]
    fn renderer_builds_platform_approval_buttons() {
        let request_id = Uuid::now_v7();
        let telegram = approval_buttons(Platform::Telegram, request_id);
        let slack = approval_buttons(Platform::Slack, request_id);
        let discord = approval_buttons(Platform::Discord, request_id);

        assert_eq!(telegram.len(), 3);
        assert_eq!(slack.len(), 3);
        assert_eq!(discord.len(), 3);
        assert!(telegram[0].label.contains("Allow"));
        assert_eq!(slack[0].label, "Allow");
        assert!(discord[2].label.contains("Deny"));
    }

    #[test]
    fn prepare_outbound_message_adds_inline_buttons_when_supported() {
        let request = approval_request();
        let message = OutboundMessage {
            content: MessageContent::ApprovalRequest {
                request: request.clone(),
            },
            buttons: Vec::new(),
            reply_to: Some("42".to_string()),
            ephemeral: false,
        };

        let prepared = prepare_outbound_message(
            Platform::Discord,
            &PlatformCapabilities {
                max_message_length: 2_000,
                supports_inline_buttons: true,
                supports_modals: true,
                supports_ephemeral: true,
                supports_threads: true,
                supports_code_blocks: true,
                supports_edit: true,
                supports_reactions: true,
                min_edit_interval: std::time::Duration::from_secs(2),
            },
            message,
        );

        assert_eq!(prepared.buttons.len(), 3);
        assert!(matches!(
            prepared.content,
            MessageContent::ApprovalRequest { .. }
        ));
    }

    #[test]
    fn prepare_outbound_message_degrades_to_text_prompt_without_buttons() {
        let request = approval_request();
        let message = OutboundMessage {
            content: MessageContent::ApprovalRequest {
                request: request.clone(),
            },
            buttons: Vec::new(),
            reply_to: Some("42".to_string()),
            ephemeral: false,
        };

        let prepared = prepare_outbound_message(
            Platform::Cli,
            &PlatformCapabilities {
                max_message_length: 2_000,
                supports_inline_buttons: false,
                supports_modals: false,
                supports_ephemeral: false,
                supports_threads: false,
                supports_code_blocks: true,
                supports_edit: false,
                supports_reactions: false,
                min_edit_interval: std::time::Duration::from_secs(0),
            },
            message,
        );

        assert!(prepared.buttons.is_empty());
        match prepared.content {
            MessageContent::Markdown(text) => {
                assert!(text.contains("/approval allow"));
                assert!(text.contains(&request.request_id.to_string()));
            }
            other => panic!("expected markdown fallback, got {other:?}"),
        }
    }
}

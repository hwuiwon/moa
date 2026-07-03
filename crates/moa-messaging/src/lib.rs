//! Messaging adapters and rendering helpers.

#[cfg(feature = "slack")]
use moa_core::{ChannelRef, InboundMessage, trace_name_from_message};

pub mod action_review;
pub mod control;
pub mod delivery;
pub mod edit_window;
#[cfg(feature = "postmark")]
pub mod postmark;
#[cfg(any(feature = "postmark", feature = "twilio"))]
mod provider_http;
pub mod rate_limit;
pub mod renderer;
#[cfg(feature = "twilio")]
pub mod twilio;

#[cfg(feature = "slack")]
pub mod slack;

pub use action_review::prepare_outbound_message;
pub use control::{MessagingControlAction, control_action_for_inbound};
pub use delivery::{
    DeliveryMessage, DeliveryPurpose, DeliveryReceipt, EnvironmentDeliveryCredentialVault,
    ProviderDeliverySink,
};
pub use edit_window::{
    MessagingEditOutcome, MessagingEditResponse, edit_with_followup_fallback,
    is_fallback_edit_error,
};
#[cfg(feature = "postmark")]
pub use postmark::{
    POSTMARK_SERVER_API_TOKEN_ENV, POSTMARK_SERVER_TOKEN_SERVICE, POSTMARK_TEST_TOKEN,
    PostmarkEmailClient, PostmarkEmailFailure, PostmarkEmailFailureClass, PostmarkEmailMessage,
    PostmarkEmailSendResult,
};
pub use rate_limit::{
    MessagingFailureClass, MessagingRateLimitMetrics, MessagingRateLimiter, MessagingSendFailure,
    MessagingSendResponse,
};
pub use renderer::SLACK_MAX_MESSAGE_LENGTH;
#[cfg(feature = "twilio")]
pub use twilio::{
    TWILIO_ACCOUNT_SID_ENV, TWILIO_ACCOUNT_SID_SERVICE, TWILIO_API_KEY_SECRET_ENV,
    TWILIO_API_KEY_SECRET_SERVICE, TWILIO_API_KEY_SID_ENV, TWILIO_API_KEY_SID_SERVICE,
    TWILIO_AUTH_TOKEN_ENV, TWILIO_AUTH_TOKEN_SERVICE, TWILIO_FROM_NUMBER_ENV,
    TWILIO_FROM_NUMBER_SERVICE, TWILIO_MESSAGING_SERVICE_SID_ENV,
    TWILIO_MESSAGING_SERVICE_SID_SERVICE, TwilioSmsClient, TwilioSmsDeliveryFailure,
    TwilioSmsFailureClass, TwilioSmsMessage, TwilioSmsSendResult,
};

#[cfg(feature = "slack")]
pub use renderer::{SlackRenderChunk, SlackRenderer};

#[cfg(feature = "slack")]
pub use slack::{SlackAdapter, SlackApiFailure, SlackApiFailureClass};

#[cfg(feature = "slack")]
pub(crate) fn messaging_receive_span(message: &InboundMessage) -> tracing::Span {
    let trace_name = trace_name_from_message(&message.text);
    let channel = message.channel.as_str();
    let route = messaging_channel_label(&message.channel_ref);
    tracing::info_span!(
        "messaging_receive",
        otel.name = %trace_name,
        moa.trace.name = %trace_name,
        moa.channel = %channel,
        moa.channel.route = %route,
        moa.channel.actor_id = %message.actor.external_id,
    )
}

#[cfg(feature = "slack")]
fn messaging_channel_label(channel_ref: &ChannelRef) -> String {
    match channel_ref {
        ChannelRef::Chat {
            conversation_id,
            client_session_id,
            ..
        } => client_session_id
            .as_ref()
            .map(|id| format!("chat:{conversation_id}:{id}"))
            .unwrap_or_else(|| format!("chat:{conversation_id}")),
        ChannelRef::Slack {
            slack_channel_id,
            thread_ts,
            user_id,
            ..
        } => match (slack_channel_id, thread_ts, user_id) {
            (Some(channel_id), Some(thread_ts), _) => {
                format!("slack_thread:{channel_id}:{thread_ts}")
            }
            (Some(channel_id), None, Some(user_id)) if channel_id.starts_with('D') => {
                format!("slack_dm:{user_id}")
            }
            (Some(channel_id), None, _) => format!("slack_channel:{channel_id}"),
            _ => "slack".to_string(),
        },
        ChannelRef::Email { channel_account_id } => format!("email:{channel_account_id}"),
        ChannelRef::Sms { channel_account_id } => format!("sms:{channel_account_id}"),
    }
}

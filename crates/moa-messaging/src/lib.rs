//! Messaging adapters and rendering helpers.

#[cfg(feature = "slack")]
use moa_core::{ChannelRef, InboundMessage, trace_name_from_message};

pub mod approval;
pub mod approval_state;
pub mod control;
pub mod edit_window;
#[cfg(feature = "postmark")]
pub mod postmark;
pub mod rate_limit;
pub mod renderer;
#[cfg(feature = "twilio")]
pub mod twilio;

#[cfg(feature = "slack")]
pub mod slack;

pub use approval::{
    ApprovalCallbackAction, approval_buttons, prepare_outbound_message, resolved_approval_buttons,
};
pub use approval_state::{
    ApprovalClickOutcome, ApprovalLifecycleState, ApprovalStateTracker, approval_state_marker,
    parse_approval_decision,
};
pub use control::{MessagingControlAction, control_action_for_inbound};
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
    TWILIO_MESSAGING_SERVICE_SID_SERVICE, TWILIO_SID_ENV, TwilioSmsClient,
    TwilioSmsDeliveryFailure, TwilioSmsFailureClass, TwilioSmsMessage, TwilioSmsSendResult,
};

#[cfg(feature = "slack")]
pub use renderer::{SlackRenderChunk, SlackRenderer};

#[cfg(feature = "slack")]
pub use slack::{SlackAdapter, SlackApiFailure, SlackApiFailureClass};

#[cfg(feature = "slack")]
pub(crate) fn messaging_receive_span(message: &InboundMessage) -> tracing::Span {
    let trace_name = trace_name_from_message(&message.text);
    let platform = message.platform.as_str();
    let channel = messaging_channel_label(&message.channel);
    let tags = format!("[\"{platform}\"]");
    tracing::info_span!(
        "messaging_receive",
        otel.name = %trace_name,
        langfuse.trace.name = %trace_name,
        langfuse.trace.tags = %tags,
        langfuse.trace.metadata.platform = %platform,
        langfuse.trace.metadata.channel = %channel,
        langfuse.trace.metadata.platform_user_id = %message.user.platform_id,
    )
}

#[cfg(feature = "slack")]
fn messaging_channel_label(channel: &ChannelRef) -> String {
    match channel {
        ChannelRef::DirectMessage { user_id } => format!("dm:{user_id}"),
        ChannelRef::Group { channel_id } => format!("group:{channel_id}"),
        ChannelRef::Thread {
            channel_id,
            thread_id,
        } => format!("thread:{channel_id}:{thread_id}"),
    }
}

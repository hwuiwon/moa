//! Messaging gateway adapters and rendering helpers.

#[cfg(feature = "slack")]
use moa_core::{ChannelRef, InboundMessage, trace_name_from_message};

pub mod approval;
pub mod approval_state;
pub mod control;
pub mod edit_window;
pub mod rate_limit;
pub mod renderer;

#[cfg(feature = "slack")]
pub mod slack;

pub use approval::{
    ApprovalCallbackAction, approval_buttons, prepare_outbound_message, resolved_approval_buttons,
};
pub use approval_state::{
    ApprovalClickOutcome, ApprovalLifecycleState, ApprovalStateTracker, approval_state_marker,
    parse_approval_decision,
};
pub use control::{GatewayControlAction, control_action_for_inbound};
pub use edit_window::{
    GatewayEditOutcome, GatewayEditResponse, edit_with_followup_fallback, is_fallback_edit_error,
};
pub use rate_limit::{GatewayRateLimitMetrics, GatewayRateLimiter, GatewaySendResponse};
pub use renderer::SLACK_MAX_MESSAGE_LENGTH;

#[cfg(feature = "slack")]
pub use renderer::{SlackRenderChunk, SlackRenderer};

#[cfg(feature = "slack")]
pub use slack::SlackAdapter;

#[cfg(feature = "slack")]
pub(crate) fn gateway_receive_span(message: &InboundMessage) -> tracing::Span {
    let trace_name = trace_name_from_message(&message.text);
    let platform = message.platform.as_str();
    let channel = gateway_channel_label(&message.channel);
    let tags = format!("[\"{platform}\"]");
    tracing::info_span!(
        "gateway_receive",
        otel.name = %trace_name,
        langfuse.trace.name = %trace_name,
        langfuse.trace.tags = %tags,
        langfuse.trace.metadata.platform = %platform,
        langfuse.trace.metadata.channel = %channel,
        langfuse.trace.metadata.platform_user_id = %message.user.platform_id,
    )
}

#[cfg(feature = "slack")]
fn gateway_channel_label(channel: &ChannelRef) -> String {
    match channel {
        ChannelRef::DirectMessage { user_id } => format!("dm:{user_id}"),
        ChannelRef::Group { channel_id } => format!("group:{channel_id}"),
        ChannelRef::Thread {
            channel_id,
            thread_id,
        } => format!("thread:{channel_id}:{thread_id}"),
    }
}

//! Messaging gateway adapters and rendering helpers.

#[cfg(any(feature = "discord", feature = "slack", feature = "telegram"))]
use moa_core::{ChannelRef, InboundMessage, trace_name_from_message};

pub mod approval;
pub mod renderer;

#[cfg(feature = "discord")]
pub mod discord;

#[cfg(feature = "slack")]
pub mod slack;

#[cfg(feature = "telegram")]
pub mod telegram;

pub use approval::{ApprovalCallbackAction, approval_buttons, prepare_outbound_message};
pub use renderer::{
    DISCORD_MAX_MESSAGE_LENGTH, SLACK_MAX_MESSAGE_LENGTH, TELEGRAM_MAX_MESSAGE_LENGTH,
};

#[cfg(feature = "discord")]
pub use discord::DiscordAdapter;

#[cfg(feature = "discord")]
pub use renderer::{DiscordRenderChunk, DiscordRenderer};

#[cfg(feature = "slack")]
pub use renderer::{SlackRenderChunk, SlackRenderer};

#[cfg(feature = "telegram")]
pub use renderer::{TelegramRenderChunk, TelegramRenderer};

#[cfg(feature = "slack")]
pub use slack::SlackAdapter;

#[cfg(feature = "telegram")]
pub use telegram::TelegramAdapter;

#[cfg(any(feature = "discord", feature = "slack", feature = "telegram"))]
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

#[cfg(any(feature = "discord", feature = "slack", feature = "telegram"))]
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

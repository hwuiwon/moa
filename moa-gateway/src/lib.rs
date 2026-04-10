//! Messaging gateway adapters and rendering helpers.

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

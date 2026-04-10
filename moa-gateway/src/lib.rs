//! Messaging gateway adapters and rendering helpers.

pub mod renderer;

#[cfg(feature = "slack")]
pub mod slack;

#[cfg(feature = "telegram")]
pub mod telegram;

pub use renderer::{SLACK_MAX_MESSAGE_LENGTH, TELEGRAM_MAX_MESSAGE_LENGTH};

#[cfg(feature = "slack")]
pub use renderer::{SlackCallbackAction, SlackRenderChunk, SlackRenderer};

#[cfg(feature = "telegram")]
pub use renderer::{TelegramCallbackAction, TelegramRenderChunk, TelegramRenderer};

#[cfg(feature = "slack")]
pub use slack::SlackAdapter;

#[cfg(feature = "telegram")]
pub use telegram::TelegramAdapter;

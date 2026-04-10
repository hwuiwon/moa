//! Messaging gateway adapters and rendering helpers.

pub mod renderer;

#[cfg(feature = "telegram")]
pub mod telegram;

pub use renderer::{
    TELEGRAM_MAX_MESSAGE_LENGTH, TelegramCallbackAction, TelegramRenderChunk, TelegramRenderer,
};

#[cfg(feature = "telegram")]
pub use telegram::TelegramAdapter;

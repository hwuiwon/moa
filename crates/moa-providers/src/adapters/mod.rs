//! Vendor-specific provider adapters.

pub mod anthropic;
pub mod gemini;
pub mod openai_chat;
pub(crate) mod openai_responses;

#[cfg(any(test, feature = "scripted-provider"))]
pub mod scripted;

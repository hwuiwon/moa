//! Messaging adapter configuration.

use serde::{Deserialize, Serialize};

/// Messaging adapter configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MessagingConfig {
    /// Slack bot token loaded from runtime configuration.
    pub slack_token: String,
    /// Slack app token loaded from runtime configuration.
    pub slack_app_token: String,
    /// Base URL for the Postmark email API.
    pub postmark_base_url: String,
    /// Default Postmark message stream.
    pub postmark_message_stream: String,
    /// Default sender address for outbound email.
    pub email_from: String,
    /// Optional reply-to address for outbound email.
    pub email_reply_to: Option<String>,
    /// Base URL for Twilio's REST API.
    pub twilio_base_url: String,
}

impl Default for MessagingConfig {
    fn default() -> Self {
        Self {
            slack_token: String::new(),
            slack_app_token: String::new(),
            postmark_base_url: "https://api.postmarkapp.com".to_string(),
            postmark_message_stream: "outbound".to_string(),
            email_from: String::new(),
            email_reply_to: None,
            twilio_base_url: "https://api.twilio.com".to_string(),
        }
    }
}

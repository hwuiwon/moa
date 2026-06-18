//! Messaging adapter configuration.

use serde::{Deserialize, Serialize};

/// Messaging adapter configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MessagingConfig {
    /// Environment variable containing the Slack bot token.
    pub slack_token_env: String,
    /// Environment variable containing the Slack app token.
    pub slack_app_token_env: String,
    /// Base URL for the Postmark email API.
    pub postmark_base_url: String,
    /// Default Postmark message stream.
    pub postmark_message_stream: String,
    /// Base URL for Twilio's REST API.
    pub twilio_base_url: String,
}

impl Default for MessagingConfig {
    fn default() -> Self {
        Self {
            slack_token_env: "SLACK_BOT_TOKEN".to_string(),
            slack_app_token_env: "SLACK_APP_TOKEN".to_string(),
            postmark_base_url: "https://api.postmarkapp.com".to_string(),
            postmark_message_stream: "outbound".to_string(),
            twilio_base_url: "https://api.twilio.com".to_string(),
        }
    }
}

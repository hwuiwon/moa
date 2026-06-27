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
            slack_token_env: "SLACK_BOT_TOKEN".to_string(),
            slack_app_token_env: "SLACK_APP_TOKEN".to_string(),
            postmark_base_url: "https://api.postmarkapp.com".to_string(),
            postmark_message_stream: "outbound".to_string(),
            email_from: String::new(),
            email_reply_to: None,
            twilio_base_url: "https://api.twilio.com".to_string(),
        }
    }
}

impl super::MoaEnvOverlay {
    /// Applies messaging adapter environment overrides.
    pub(in crate::config) fn apply_messaging_overlay(&self, config: &mut super::MoaConfig) {
        use super::env_overlay::{set_if_some, set_option_if_some};

        set_if_some(
            &mut config.messaging.slack_token_env,
            &self.messaging_slack_token_env,
        );
        set_if_some(
            &mut config.messaging.slack_app_token_env,
            &self.messaging_slack_app_token_env,
        );
        set_if_some(
            &mut config.messaging.postmark_base_url,
            &self.messaging_postmark_base_url,
        );
        set_if_some(
            &mut config.messaging.postmark_message_stream,
            &self.messaging_postmark_message_stream,
        );
        set_if_some(&mut config.messaging.email_from, &self.messaging_email_from);
        set_option_if_some(
            &mut config.messaging.email_reply_to,
            &self.messaging_email_reply_to,
        );
        set_if_some(
            &mut config.messaging.twilio_base_url,
            &self.messaging_twilio_base_url,
        );
    }
}

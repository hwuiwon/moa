//! Messaging gateway configuration.

use serde::{Deserialize, Serialize};

/// Messaging gateway configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayConfig {
    /// Environment variable containing the Telegram bot token.
    pub telegram_token_env: String,
    /// Environment variable containing the Slack bot token.
    pub slack_token_env: String,
    /// Environment variable containing the Slack app token.
    pub slack_app_token_env: String,
    /// Environment variable containing the Discord bot token.
    pub discord_token_env: String,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            telegram_token_env: "TELEGRAM_BOT_TOKEN".to_string(),
            slack_token_env: "SLACK_BOT_TOKEN".to_string(),
            slack_app_token_env: "SLACK_APP_TOKEN".to_string(),
            discord_token_env: "DISCORD_BOT_TOKEN".to_string(),
        }
    }
}

//! Local, cloud hand, and MCP sandbox configuration.

use serde::{Deserialize, Serialize};

/// Local runtime configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalConfig {
    /// Whether local Docker hands are enabled.
    pub docker_enabled: bool,
    /// Sandbox working directory.
    pub sandbox_dir: String,
    /// Memory root directory.
    pub memory_dir: String,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            docker_enabled: true,
            sandbox_dir: "~/.moa/sandbox".to_string(),
            memory_dir: "~/.moa/memory".to_string(),
        }
    }
}

/// Cloud runtime configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudConfig {
    /// Optional alternate memory root for cloud deployments.
    pub memory_dir: Option<String>,
    /// Optional hands configuration.
    pub hands: Option<CloudHandsConfig>,
}

impl Default for CloudConfig {
    fn default() -> Self {
        Self {
            memory_dir: None,
            hands: Some(CloudHandsConfig::default()),
        }
    }
}

/// Cloud hand provider configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudHandsConfig {
    /// Default hand provider.
    pub default_provider: Option<String>,
    /// Daytona API key loaded from runtime configuration.
    pub daytona_api_key: Option<String>,
    /// Optional Daytona API base URL override.
    pub daytona_api_url: Option<String>,
    /// Optional default image for Daytona sandboxes.
    pub daytona_default_image: Option<String>,
    /// E2B API key loaded from runtime configuration.
    pub e2b_api_key: Option<String>,
    /// Optional E2B API base URL override.
    pub e2b_api_url: Option<String>,
    /// Optional E2B domain override.
    pub e2b_domain: Option<String>,
    /// Optional default E2B template identifier.
    pub e2b_template: Option<String>,
}

/// Supported MCP transport configurations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum McpTransportConfig {
    /// Launch a local MCP server over stdio.
    #[default]
    Stdio,
    /// Connect to a server-sent-event MCP endpoint.
    Sse,
    /// Connect to a Streamable HTTP MCP endpoint.
    Http,
}

/// Credential injection mode for an MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpCredentialConfig {
    /// Attach a bearer token from an environment variable.
    Bearer {
        /// Environment variable containing the token.
        token_env: String,
    },
    /// Attach an OAuth access token from an environment variable.
    OAuth {
        /// Environment variable containing the access token.
        token_env: String,
    },
    /// Attach an API key header from an environment variable.
    ApiKey {
        /// Header name expected by the upstream service.
        header: String,
        /// Environment variable containing the header value.
        value_env: String,
    },
}

/// One configured MCP server connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct McpServerConfig {
    /// Stable MCP server name.
    pub name: String,
    /// Selected transport for the server.
    pub transport: McpTransportConfig,
    /// Optional stdio command.
    pub command: Option<String>,
    /// Optional stdio command arguments.
    pub args: Vec<String>,
    /// Optional stdio environment variables.
    pub env: std::collections::HashMap<String, String>,
    /// Optional remote endpoint URL for HTTP/SSE transports.
    pub url: Option<String>,
    /// Optional credential injection configuration.
    pub credentials: Option<McpCredentialConfig>,
}

impl super::MoaEnvOverlay {
    /// Applies local runtime environment overrides.
    pub(in crate::config) fn apply_local_overlay(&self, config: &mut super::MoaConfig) {
        use super::env_overlay::{set_copy_if_some, set_if_some};

        set_copy_if_some(&mut config.local.docker_enabled, self.local_docker_enabled);
        set_if_some(&mut config.local.sandbox_dir, &self.local_sandbox_dir);
        set_if_some(&mut config.local.memory_dir, &self.local_memory_dir);
    }

    /// Applies cloud runtime and cloud hands environment overrides.
    pub(in crate::config) fn apply_cloud_overlay(&self, config: &mut super::MoaConfig) {
        use super::env_overlay::{any_present, set_option_if_some};

        set_option_if_some(&mut config.cloud.memory_dir, &self.cloud_memory_dir);
        if any_present(&[
            self.cloud_hands_default_provider.is_some(),
            self.cloud_hands_daytona_api_key.is_some(),
            self.cloud_hands_daytona_api_url.is_some(),
            self.cloud_hands_daytona_default_image.is_some(),
            self.cloud_hands_e2b_api_key.is_some(),
            self.cloud_hands_e2b_api_url.is_some(),
            self.cloud_hands_e2b_domain.is_some(),
            self.cloud_hands_e2b_template.is_some(),
        ]) {
            let hands = config
                .cloud
                .hands
                .get_or_insert_with(CloudHandsConfig::default);
            set_option_if_some(
                &mut hands.default_provider,
                &self.cloud_hands_default_provider,
            );
            set_option_if_some(
                &mut hands.daytona_api_key,
                &self.cloud_hands_daytona_api_key,
            );
            set_option_if_some(
                &mut hands.daytona_api_url,
                &self.cloud_hands_daytona_api_url,
            );
            set_option_if_some(
                &mut hands.daytona_default_image,
                &self.cloud_hands_daytona_default_image,
            );
            set_option_if_some(&mut hands.e2b_api_key, &self.cloud_hands_e2b_api_key);
            set_option_if_some(&mut hands.e2b_api_url, &self.cloud_hands_e2b_api_url);
            set_option_if_some(&mut hands.e2b_domain, &self.cloud_hands_e2b_domain);
            set_option_if_some(&mut hands.e2b_template, &self.cloud_hands_e2b_template);
        }
    }
}

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
    /// Whether cloud mode is enabled.
    pub enabled: bool,
    /// Optional alternate memory root for cloud deployments.
    pub memory_dir: Option<String>,
    /// Optional Fly.io configuration.
    pub flyio: Option<CloudFlyioConfig>,
    /// Optional hands configuration.
    pub hands: Option<CloudHandsConfig>,
}

impl Default for CloudConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            memory_dir: None,
            flyio: Some(CloudFlyioConfig::default()),
            hands: Some(CloudHandsConfig::default()),
        }
    }
}

/// Fly.io configuration for cloud mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudFlyioConfig {
    /// Environment variable containing the Fly.io API token.
    pub api_token_env: Option<String>,
    /// Fly application name.
    pub app_name: Option<String>,
    /// Primary region.
    pub region: String,
    /// Internal HTTP port used for Fly health checks.
    pub internal_port: u16,
    /// Interface used by the cloud health endpoint.
    pub health_bind: String,
    /// Grace period for active turns to complete after SIGTERM.
    pub graceful_shutdown_timeout_secs: u64,
}

impl Default for CloudFlyioConfig {
    fn default() -> Self {
        Self {
            api_token_env: Some("FLY_API_TOKEN".to_string()),
            app_name: None,
            region: "iad".to_string(),
            internal_port: 8080,
            health_bind: "0.0.0.0".to_string(),
            graceful_shutdown_timeout_secs: 30,
        }
    }
}

/// Cloud hand provider configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudHandsConfig {
    /// Default hand provider.
    pub default_provider: Option<String>,
    /// Environment variable containing the Daytona API key.
    pub daytona_api_key_env: Option<String>,
    /// Optional Daytona API base URL override.
    pub daytona_api_url: Option<String>,
    /// Optional default image for Daytona sandboxes.
    pub daytona_default_image: Option<String>,
    /// Environment variable containing the E2B API key.
    pub e2b_api_key_env: Option<String>,
    /// Optional E2B API base URL override.
    pub e2b_api_url: Option<String>,
    /// Optional E2B domain override.
    pub e2b_domain: Option<String>,
    /// Optional default E2B template identifier.
    pub e2b_template: Option<String>,
}

impl Default for CloudHandsConfig {
    fn default() -> Self {
        Self {
            default_provider: Some("daytona".to_string()),
            daytona_api_key_env: Some("DAYTONA_API_KEY".to_string()),
            daytona_api_url: Some("https://app.daytona.io/api".to_string()),
            daytona_default_image: Some("daytonaio/workspace:latest".to_string()),
            e2b_api_key_env: Some("E2B_API_KEY".to_string()),
            e2b_api_url: Some("https://api.e2b.dev".to_string()),
            e2b_domain: Some("e2b.app".to_string()),
            e2b_template: Some("base".to_string()),
        }
    }
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

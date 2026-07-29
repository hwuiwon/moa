//! Deployment credential loading for outbound MCP requests.

use std::collections::HashMap;

use moa_config::{McpCredentialConfig, McpServerConfig};
use moa_core::{error::MoaError, error::Result};
use secrecy::{ExposeSecret, SecretString};

/// Deployment-owned credentials for configured MCP servers.
///
/// Values are read once when the tool router is constructed. A server that
/// declares a credential whose environment variable is absent fails closed.
#[derive(Default)]
pub struct McpDeploymentCredentials {
    by_server: HashMap<String, SecretString>,
}

impl McpDeploymentCredentials {
    /// Loads each configured MCP server credential from the deployment environment.
    pub fn from_mcp_servers(servers: &[McpServerConfig]) -> Result<Self> {
        let mut by_server = HashMap::new();
        for server in servers {
            if let Some(config) = server.credentials.as_ref() {
                by_server.insert(server.name.clone(), credential_from_env(config)?);
            }
        }
        Ok(Self { by_server })
    }

    /// Returns the outbound authentication headers for `server`.
    pub fn headers_for(&self, server: &McpServerConfig) -> Result<HashMap<String, String>> {
        let Some(config) = server.credentials.as_ref() else {
            return Ok(HashMap::new());
        };
        let secret = self.by_server.get(&server.name).ok_or_else(|| {
            MoaError::ConfigError(format!(
                "MCP server '{}' has no deployment credential configured",
                server.name
            ))
        })?;

        let mut headers = HashMap::new();
        match config {
            McpCredentialConfig::ApiKey { header, .. } => {
                headers.insert(header.clone(), secret.expose_secret().to_string());
            }
            McpCredentialConfig::Bearer { .. } => {
                headers.insert(
                    "Authorization".to_string(),
                    format!("Bearer {}", secret.expose_secret()),
                );
            }
        }
        Ok(headers)
    }
}

impl std::fmt::Debug for McpDeploymentCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut configured: Vec<&str> = self.by_server.keys().map(String::as_str).collect();
        configured.sort_unstable();
        formatter
            .debug_struct("McpDeploymentCredentials")
            .field("configured", &configured)
            .finish()
    }
}

fn credential_from_env(config: &McpCredentialConfig) -> Result<SecretString> {
    let name = match config {
        McpCredentialConfig::Bearer { token_env } => token_env,
        McpCredentialConfig::ApiKey { value_env, .. } => value_env,
    };
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(SecretString::from)
        .ok_or_else(|| MoaError::MissingEnvironmentVariable(name.clone()))
}

#[cfg(test)]
mod tests;

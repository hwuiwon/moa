//! Session-scoped credential resolution for MCP-backed tool calls.

use std::collections::HashMap;
use std::sync::Arc;

use moa_config::{McpCredentialConfig, McpServerConfig};
use moa_core::{
    error::MoaError, error::Result, traits::CredentialVault, types::credentials::CredentialContext,
    types::credentials::CredentialError, types::credentials::CredentialSource,
    types::credentials::RedactedSecret, types::identifiers::SessionId,
};
use secrecy::{ExposeSecret, SecretString};

/// Deployment-owned credentials for explicitly deployment-owned MCP servers.
///
/// These are not tenant credentials and deliberately never reach the vault: the
/// operator configured one credential for one server in deployment config, so
/// the material is deployment-scoped exactly like an email or SMS transport
/// secret. Unlike those transports the set is open-ended — an operator can
/// configure any number of servers — so it is keyed by the operator-authored
/// server name rather than by a closed enum. The name is never caller-supplied.
///
/// Every value is read once at construction and fails closed: a server that
/// declares a credential whose environment variable is unset is a configuration
/// error, not a server that silently dispatches unauthenticated.
#[derive(Default)]
pub struct McpDeploymentCredentials {
    by_server: HashMap<String, SecretString>,
}

impl McpDeploymentCredentials {
    /// Reads the configured credential for every server that declares one.
    pub fn from_mcp_servers(servers: &[McpServerConfig]) -> Result<Self> {
        let mut by_server = HashMap::new();
        for server in servers {
            let Some(config) = &server.credentials else {
                continue;
            };
            by_server.insert(server.name.clone(), credential_from_env(config)?);
        }
        Ok(Self { by_server })
    }

    /// Returns whether a deployment credential is configured for `server`.
    #[must_use]
    pub fn contains(&self, server: &str) -> bool {
        self.by_server.contains_key(server)
    }

    /// Resolves one server's deployment credential for an outbound request.
    fn resolve(&self, server: &str) -> Result<RedactedSecret> {
        self.by_server
            .get(server)
            .map(|value| RedactedSecret::new(value.expose_secret().to_string()))
            .ok_or_else(|| {
                MoaError::ConfigError(format!(
                    "MCP server '{server}' has no deployment credential configured"
                ))
            })
    }
}

/// Reads one MCP credential's configured environment variable.
fn credential_from_env(config: &McpCredentialConfig) -> Result<SecretString> {
    let name = match config {
        McpCredentialConfig::Bearer { token_env } | McpCredentialConfig::OAuth { token_env } => {
            token_env
        }
        McpCredentialConfig::ApiKey { value_env, .. } => value_env,
    };
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(SecretString::from)
        .ok_or_else(|| MoaError::MissingEnvironmentVariable(name.clone()))
}

/// MCP credential resolver that reads real credentials only at call time.
///
/// This is the outbound-to-external-MCP boundary. It owns both ownership
/// branches so a call site cannot reach one while believing it used the other:
/// deployment-owned servers resolve from [`McpDeploymentCredentials`], and
/// tenant-owned servers resolve the exact stored version from the durable tenant
/// credential owner. Data-class egress governance lives on the tool router in
/// `moa-hands`, which drives [`crate::mcp_egress::McpEgressGuard`] directly
/// before dispatch — not here.
pub struct MCPCredentialProxy {
    vault: Option<Arc<dyn CredentialVault>>,
    deployment: McpDeploymentCredentials,
}

impl MCPCredentialProxy {
    /// Creates an MCP credential resolver backed by the durable tenant vault.
    pub fn new(vault: Arc<dyn CredentialVault>) -> Self {
        Self {
            vault: Some(vault),
            deployment: McpDeploymentCredentials::default(),
        }
    }

    /// Creates a resolver for a deployment that has no tenant-owned MCP servers.
    ///
    /// The absent vault is not a fallback: it makes tenant resolution impossible
    /// rather than silently satisfying it from deployment material.
    #[must_use]
    pub fn deployment_only(deployment: McpDeploymentCredentials) -> Self {
        Self {
            vault: None,
            deployment,
        }
    }

    /// Attaches the deployment-owned MCP server credentials.
    #[must_use]
    pub fn with_deployment_credentials(mut self, deployment: McpDeploymentCredentials) -> Self {
        self.deployment = deployment;
        self
    }

    /// Resolves credential headers for one deployment-owned MCP server.
    ///
    /// The server name selects operator-authored deployment configuration, so no
    /// tenant credential is consulted and no tenant state can influence which
    /// credential is used.
    pub fn deployment_headers(
        &self,
        session_id: &SessionId,
        server: &str,
        config: Option<&McpCredentialConfig>,
    ) -> Result<HashMap<String, String>> {
        let secret = self.deployment.resolve(server)?;
        tracing::debug!(
            %session_id,
            server,
            "resolved deployment-owned MCP credential headers"
        );
        Ok(headers_from_secret(config, &secret))
    }

    /// Resolves credential headers for one MCP call from the durable vault.
    ///
    /// The caller supplies the typed [`CredentialSource`] selecting which stored
    /// credential to use and the [`CredentialContext`] carrying the acting
    /// principal, requested operation, and replay-stable operation identity. The
    /// plaintext exists only inside this call: it is resolved as a
    /// [`RedactedSecret`], shaped into headers, and dropped.
    ///
    /// No proxy token is minted here. The previous design minted an opaque token
    /// and consumed it inside this same host function, so the token added cache,
    /// expiry, and allocation cost without ever crossing an isolation boundary.
    /// Reintroduce a single-use token returned from this call — bound to
    /// `session_id`, the source, an expiry, and one use — only when a real remote
    /// proxy boundary sits between this resolver and the MCP transport that
    /// consumes the credential.
    pub async fn enrich_headers(
        &self,
        session_id: &SessionId,
        source: &CredentialSource,
        config: Option<&McpCredentialConfig>,
        ctx: &CredentialContext,
    ) -> Result<HashMap<String, String>> {
        let vault = self.vault.as_ref().ok_or_else(|| {
            MoaError::ConfigError(
                "tenant-owned MCP resolution requires an attached credential vault".to_string(),
            )
        })?;
        let secret = vault.resolve(source, ctx).await.map_err(vault_error)?;
        tracing::debug!(
            %session_id,
            operation = ctx.operation.as_str(),
            "resolved MCP credential headers from the tenant credential vault"
        );
        Ok(headers_from_secret(config, &secret))
    }
}

/// Maps a typed vault failure onto the tool-call error surface.
///
/// Every [`CredentialError`] `Display` is already secret-free, so the reason can
/// be carried through verbatim. The variant split matters for the caller: a
/// storage or key-management outage is transient infrastructure, while every
/// selector/authorization failure is a closed denial that must not look
/// retryable.
fn vault_error(error: CredentialError) -> MoaError {
    match error {
        CredentialError::Storage(_) => MoaError::StorageError(error.to_string()),
        CredentialError::DeploymentSecretMissing => MoaError::ConfigError(error.to_string()),
        CredentialError::IdempotencyConflict | CredentialError::VersionConflict => {
            MoaError::ValidationError(error.to_string())
        }
        CredentialError::NotFound
        | CredentialError::Revoked
        | CredentialError::StaleVersion
        | CredentialError::WrongTenant
        | CredentialError::WrongConnection
        | CredentialError::WrongKind
        | CredentialError::Unauthorized => MoaError::PermissionDenied(error.to_string()),
    }
}

/// Shapes one resolved secret into outbound headers.
///
/// The header shape comes from the server's configured credential mode, so the
/// secret stays an opaque string and is exposed exactly once, at the point it is
/// written into the header value.
fn headers_from_secret(
    config: Option<&McpCredentialConfig>,
    secret: &RedactedSecret,
) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    match config {
        Some(McpCredentialConfig::ApiKey { header, .. }) => {
            headers.insert(
                header.clone(),
                secret.expose_for_outbound_request().to_string(),
            );
        }
        Some(McpCredentialConfig::Bearer { .. } | McpCredentialConfig::OAuth { .. }) => {
            headers.insert(
                "Authorization".to_string(),
                format!("Bearer {}", secret.expose_for_outbound_request()),
            );
        }
        None => {}
    }
    headers
}

#[cfg(test)]
mod tests;

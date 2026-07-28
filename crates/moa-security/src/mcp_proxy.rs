//! Session-scoped credential resolution for MCP-backed tool calls.

use std::collections::HashMap;
use std::sync::Arc;

use moa_config::{McpCredentialConfig, McpServerConfig, McpServerCredentialScope};
use moa_core::{
    error::MoaError, error::Result, traits::CredentialVault, types::credentials::CredentialContext,
    types::credentials::CredentialError, types::credentials::CredentialIdentity,
    types::credentials::CredentialRef, types::credentials::CredentialSource,
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
    /// Validates every server's credential ownership and reads the deployment
    /// credential for each explicitly deployment-owned server that declares one.
    ///
    /// This is the one place the two ownership branches are separated, and it
    /// fails closed on every incoherent combination:
    ///
    /// - a deployment-owned server whose configuration carries only a header
    ///   shape has no material to read;
    /// - a tenant-owned server that names a deployment environment variable is
    ///   rejected outright, so environment material can never be reachable from a
    ///   tenant-owned connector;
    /// - a tenant-owned server that declares no credential has no header to
    ///   present its tenant's resolved secret in.
    pub fn from_mcp_servers(servers: &[McpServerConfig]) -> Result<Self> {
        let mut by_server = HashMap::new();
        for server in servers {
            match (server.credential_scope, server.credentials.as_ref()) {
                // A deployment-owned server may legitimately need no credential
                // at all (an unauthenticated internal MCP endpoint).
                (McpServerCredentialScope::DeploymentOwned, None) => {}
                (McpServerCredentialScope::DeploymentOwned, Some(config))
                    if config.is_deployment_selector() =>
                {
                    by_server.insert(server.name.clone(), credential_from_env(config)?);
                }
                (McpServerCredentialScope::DeploymentOwned, Some(_)) => {
                    return Err(MoaError::ConfigError(format!(
                        "deployment-owned MCP server '{}' must name a deployment environment \
                         variable; tenant header shapes have no deployment material",
                        server.name
                    )));
                }
                (McpServerCredentialScope::TenantOwned, Some(config))
                    if config.is_tenant_header_shape() => {}
                (McpServerCredentialScope::TenantOwned, Some(_)) => {
                    return Err(MoaError::ConfigError(format!(
                        "tenant-owned MCP server '{}' must not name a deployment environment \
                         variable; its credential is resolved per tenant from the credential vault",
                        server.name
                    )));
                }
                (McpServerCredentialScope::TenantOwned, None) => {
                    return Err(MoaError::ConfigError(format!(
                        "tenant-owned MCP server '{}' must declare the header shape its tenant \
                         credential is presented in",
                        server.name
                    )));
                }
            }
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

impl std::fmt::Debug for McpDeploymentCredentials {
    /// Renders only which servers have a deployment credential, never any value.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut configured: Vec<&str> = self.by_server.keys().map(String::as_str).collect();
        configured.sort_unstable();
        formatter
            .debug_struct("McpDeploymentCredentials")
            .field("configured", &configured)
            .finish()
    }
}

/// Reads one MCP credential's configured environment variable.
///
/// Only a deployment selector names one. A tenant header shape reaching here is
/// a configuration error rather than a silent empty credential, which keeps
/// environment material unreachable from a tenant-owned connector even if a
/// future caller skips the ownership validation above.
fn credential_from_env(config: &McpCredentialConfig) -> Result<SecretString> {
    let name = match config {
        McpCredentialConfig::Bearer { token_env } | McpCredentialConfig::OAuth { token_env } => {
            token_env
        }
        McpCredentialConfig::ApiKey { value_env, .. } => value_env,
        McpCredentialConfig::TenantBearer | McpCredentialConfig::TenantApiKey { .. } => {
            return Err(MoaError::ConfigError(
                "tenant-owned MCP credentials are never read from deployment environment"
                    .to_string(),
            ));
        }
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

    /// Returns whether this resolver can serve tenant-owned MCP servers.
    ///
    /// Runtime composition uses this to refuse to build a router that configures
    /// a tenant-owned server without the durable credential owner, instead of
    /// discovering it on the first tenant dispatch.
    #[must_use]
    pub fn serves_tenant_owned(&self) -> bool {
        self.vault.is_some()
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
        config: &McpCredentialConfig,
    ) -> Result<HashMap<String, String>> {
        let secret = self.deployment.resolve(server)?;
        tracing::debug!(
            %session_id,
            server,
            "resolved deployment-owned MCP credential headers"
        );
        Ok(headers_from_secret(config, &secret))
    }

    /// Resolves credential headers for one tenant-owned MCP call from the vault.
    ///
    /// `expected` is the identity the caller's binding says the reference belongs
    /// to. It is verified against the stored version — without opening any
    /// material — before the audited resolve, so a reference that has drifted
    /// onto another tenant's connection or another material kind is refused
    /// before plaintext exists. The vault re-checks tenant ownership, revocation,
    /// and active status on the resolve itself, so this check narrows the
    /// selector rather than replacing the owner's own gates.
    ///
    /// The plaintext exists only inside this call: it is resolved as a
    /// [`RedactedSecret`], shaped into headers, and dropped. Nothing here can
    /// reach the deployment branch — a tenant-owned failure is a failure, never a
    /// silent downgrade to the operator's credential.
    ///
    /// No proxy token is minted here. The previous design minted an opaque token
    /// and consumed it inside this same host function, so the token added cache,
    /// expiry, and allocation cost without ever crossing an isolation boundary.
    /// Reintroduce a single-use token returned from this call — bound to
    /// `session_id`, the source, an expiry, and one use — only when a real remote
    /// proxy boundary sits between this resolver and the MCP transport that
    /// consumes the credential.
    pub async fn tenant_headers(
        &self,
        session_id: &SessionId,
        expected: CredentialIdentity,
        reference: CredentialRef,
        config: &McpCredentialConfig,
        ctx: &CredentialContext,
    ) -> Result<HashMap<String, String>> {
        let vault = self.vault.as_ref().ok_or_else(|| {
            MoaError::ConfigError(
                "tenant-owned MCP resolution requires an attached credential vault".to_string(),
            )
        })?;
        if expected.tenant_id != ctx.tenant_id {
            return Err(MoaError::PermissionDenied(
                "MCP credential binding does not belong to the resolving tenant".to_string(),
            ));
        }
        let stored = vault.describe(reference, ctx).await.map_err(vault_error)?;
        if stored.identity != expected {
            return Err(vault_error(
                if stored.identity.tenant_id != expected.tenant_id {
                    CredentialError::WrongTenant
                } else if stored.identity.connection_uid != expected.connection_uid {
                    CredentialError::WrongConnection
                } else {
                    CredentialError::WrongKind
                },
            ));
        }
        let secret = vault
            .resolve(&CredentialSource::TenantConnection { reference }, ctx)
            .await
            .map_err(vault_error)?;
        tracing::debug!(
            %session_id,
            operation = ctx.operation.as_str(),
            connection_uid = %expected.connection_uid,
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
/// The header shape comes from the server's configured credential mode and is
/// independent of which owner supplied the material, so the secret stays an
/// opaque string and is exposed exactly once, at the point it is written into
/// the header value.
fn headers_from_secret(
    config: &McpCredentialConfig,
    secret: &RedactedSecret,
) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    match config {
        McpCredentialConfig::ApiKey { header, .. }
        | McpCredentialConfig::TenantApiKey { header } => {
            headers.insert(
                header.clone(),
                secret.expose_for_outbound_request().to_string(),
            );
        }
        McpCredentialConfig::Bearer { .. }
        | McpCredentialConfig::OAuth { .. }
        | McpCredentialConfig::TenantBearer => {
            headers.insert(
                "Authorization".to_string(),
                format!("Bearer {}", secret.expose_for_outbound_request()),
            );
        }
    }
    headers
}

#[cfg(test)]
mod tests;

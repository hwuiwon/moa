//! Local, cloud hand, and MCP sandbox configuration.

use serde::{Deserialize, Serialize};

use moa_core::types::security::SensitivityClass;

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
    /// Ordered fallback cloud providers attempted when the selected cloud hand is unavailable.
    pub fallback_providers: Vec<String>,
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
    /// Connect to a server-sent-event MCP endpoint.
    Sse,
    /// Connect to a Streamable HTTP MCP endpoint.
    #[default]
    Http,
}

/// Who owns the credential an MCP server is invoked with.
///
/// This is the single ownership switch for outbound MCP traffic and has no
/// default: an operator must say, per server, whether every tenant shares one
/// deployment credential or whether each tenant presents its own. A server that
/// omits it is a typed configuration error, because guessing either way is a
/// security decision MOA is not entitled to make on the operator's behalf.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerCredentialScope {
    /// One operator-owned credential, read from deployment environment, used for
    /// every tenant that invokes this server.
    DeploymentOwned,
    /// Each tenant presents its own connection credential, resolved per call
    /// from the durable tenant credential vault through its connection binding.
    TenantOwned,
}

impl McpServerCredentialScope {
    /// Returns the stable configuration/audit name for this scope.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeploymentOwned => "deployment_owned",
            Self::TenantOwned => "tenant_owned",
        }
    }
}

/// Credential injection mode for an MCP server.
///
/// The three environment variants are *deployment selectors*: they name an
/// operator-authored environment variable that is read once at construction. The
/// two tenant variants carry only the outbound header shape; the material never
/// appears in configuration and is resolved per call from the durable tenant
/// credential vault. Mixing the two — a deployment selector on a tenant-owned
/// server, or a header-only shape on a deployment-owned one — is rejected when
/// the router is constructed, so no dispatch can quietly use the wrong owner's
/// material.
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
    /// Attach the tenant's vault-resolved credential as a bearer token.
    TenantBearer,
    /// Attach the tenant's vault-resolved credential as an API key header.
    TenantApiKey {
        /// Header name expected by the upstream service.
        header: String,
    },
}

impl McpCredentialConfig {
    /// Returns whether this configuration selects a deployment environment
    /// variable as the credential source.
    #[must_use]
    pub fn is_deployment_selector(&self) -> bool {
        matches!(
            self,
            Self::Bearer { .. } | Self::OAuth { .. } | Self::ApiKey { .. }
        )
    }

    /// Returns whether this configuration carries only an outbound header shape,
    /// leaving the material to be resolved per call from the tenant vault.
    #[must_use]
    pub fn is_tenant_header_shape(&self) -> bool {
        matches!(self, Self::TenantBearer | Self::TenantApiKey { .. })
    }
}

/// One configured MCP server connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Stable MCP server name.
    pub name: String,
    /// Selected transport for the server.
    pub transport: McpTransportConfig,
    /// Optional remote endpoint URL for HTTP/SSE transports.
    pub url: Option<String>,
    /// Which owner's credential this server is invoked with.
    pub credential_scope: McpServerCredentialScope,
    /// Optional credential injection configuration.
    ///
    /// `None` is legal only for a deployment-owned server that needs no
    /// credential at all; a tenant-owned server must declare the header shape
    /// its resolved credential is presented in.
    pub credentials: Option<McpCredentialConfig>,
    /// Whether standard MCP tool annotations from this server may affect retry safety.
    #[serde(default)]
    pub trust_tool_annotations: bool,
    /// Data classes this external MCP server is permitted to receive.
    ///
    /// This is a conservative egress allowlist. When empty (the default for an
    /// existing config), only [`SensitivityClass::None`] content may be sent to
    /// the server; `pii`, `phi`, and `restricted` payloads are blocked unless the
    /// operator explicitly lists them here.
    #[serde(default)]
    pub allowed_data_classes: Vec<SensitivityClass>,
}

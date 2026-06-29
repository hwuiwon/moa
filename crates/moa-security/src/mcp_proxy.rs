//! Session-scoped credential proxying for MCP-backed tool calls.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::{
    Credential, CredentialVault, McpCredentialConfig, McpServerConfig, MoaError, Result, SessionId,
};
use tokio::sync::RwLock;
use uuid::Uuid;

const DEFAULT_TOKEN_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone)]
struct ProxyGrant {
    service: String,
    scope: String,
    expires_at: DateTime<Utc>,
}

/// Session-scoped opaque token for one proxied MCP credential grant.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct McpSessionToken(String);

impl McpSessionToken {
    /// Returns the opaque token string sent to the proxy boundary.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// MCP credential proxy that resolves real credentials via a vault only at call time.
pub struct MCPCredentialProxy {
    vault: Arc<dyn CredentialVault>,
    session_tokens: RwLock<HashMap<String, ProxyGrant>>,
    token_ttl: Duration,
}

impl MCPCredentialProxy {
    /// Creates a new MCP credential proxy.
    pub fn new(vault: Arc<dyn CredentialVault>) -> Self {
        Self {
            vault,
            session_tokens: RwLock::new(HashMap::new()),
            token_ttl: DEFAULT_TOKEN_TTL,
        }
    }

    /// Overrides the default token time-to-live.
    pub fn with_token_ttl(mut self, token_ttl: Duration) -> Self {
        self.token_ttl = token_ttl;
        self
    }

    /// Creates a new session-scoped opaque token for one service and scope.
    pub async fn create_session_token(
        &self,
        session_id: &SessionId,
        service: impl Into<String>,
        scope: impl Into<String>,
    ) -> Result<McpSessionToken> {
        self.prune_expired_tokens().await;
        let token = format!("mcp_{}_{}", session_id, Uuid::now_v7());
        self.session_tokens.write().await.insert(
            token.clone(),
            ProxyGrant {
                service: service.into(),
                scope: scope.into(),
                expires_at: Utc::now()
                    + chrono::Duration::from_std(self.token_ttl).map_err(|error| {
                        MoaError::ValidationError(format!("invalid MCP proxy token ttl: {error}"))
                    })?,
            },
        );
        Ok(McpSessionToken(token))
    }

    /// Revokes a previously issued proxy token before it is used.
    pub async fn revoke_session_token(&self, token: &McpSessionToken) {
        self.session_tokens.write().await.remove(token.as_str());
    }

    /// Resolves and injects credential headers for one opaque proxy token.
    ///
    /// The proxy grant is consumed before the vault lookup so grants cannot be
    /// reused across requests while MOA only has a process-local grant store.
    pub async fn enrich_headers(
        &self,
        token: &McpSessionToken,
        config: Option<&McpCredentialConfig>,
    ) -> Result<HashMap<String, String>> {
        let grant = self.lookup_grant(token).await?;
        let credential = self.vault.get(&grant.service, &grant.scope).await?;
        Ok(headers_from_credential(config, credential))
    }

    async fn lookup_grant(&self, token: &McpSessionToken) -> Result<ProxyGrant> {
        self.prune_expired_tokens().await;
        self.session_tokens
            .write()
            .await
            .remove(token.as_str())
            .ok_or_else(|| {
                MoaError::PermissionDenied("unknown or expired MCP proxy token".to_string())
            })
    }

    async fn prune_expired_tokens(&self) {
        let now = Utc::now();
        self.session_tokens
            .write()
            .await
            .retain(|_, grant| grant.expires_at > now);
    }
}

/// Environment-backed credential vault built from MCP server configuration.
pub struct EnvironmentCredentialVault {
    credentials: RwLock<HashMap<(String, String), Credential>>,
}

impl EnvironmentCredentialVault {
    /// Builds an environment-backed vault from configured MCP servers.
    pub fn from_mcp_servers(servers: &[McpServerConfig]) -> Result<Self> {
        let mut credentials = HashMap::new();
        for server in servers {
            let Some(config) = &server.credentials else {
                continue;
            };
            let credential = credential_from_env(config)?;
            credentials.insert((server.name.clone(), server.name.clone()), credential);
        }
        Ok(Self {
            credentials: RwLock::new(credentials),
        })
    }
}

#[async_trait]
impl CredentialVault for EnvironmentCredentialVault {
    async fn get(&self, service: &str, scope: &str) -> Result<Credential> {
        self.credentials
            .read()
            .await
            .get(&(service.to_string(), scope.to_string()))
            .cloned()
            .ok_or_else(|| {
                MoaError::MissingEnvironmentVariable(format!(
                    "credential not configured for service {service} scope {scope}"
                ))
            })
    }

    async fn set(&self, service: &str, scope: &str, cred: Credential) -> Result<()> {
        self.credentials
            .write()
            .await
            .insert((service.to_string(), scope.to_string()), cred);
        Ok(())
    }

    async fn delete(&self, service: &str, scope: &str) -> Result<()> {
        self.credentials
            .write()
            .await
            .remove(&(service.to_string(), scope.to_string()));
        Ok(())
    }

    async fn list(&self, scope: &str) -> Result<Vec<String>> {
        Ok(self
            .credentials
            .read()
            .await
            .keys()
            .filter(|(_, candidate_scope)| candidate_scope == scope)
            .map(|(service, _)| service.clone())
            .collect())
    }
}

fn credential_from_env(config: &McpCredentialConfig) -> Result<Credential> {
    match config {
        McpCredentialConfig::Bearer { token_env } => Ok(Credential::Bearer(env_var(token_env)?)),
        McpCredentialConfig::OAuth { token_env } => Ok(Credential::OAuth {
            access_token: env_var(token_env)?,
            refresh_token: None,
            expires_at: None,
        }),
        McpCredentialConfig::ApiKey { header, value_env } => Ok(Credential::ApiKey {
            header: header.clone(),
            value: env_var(value_env)?,
        }),
    }
}

fn env_var(name: &str) -> Result<String> {
    std::env::var(name).map_err(|_| MoaError::MissingEnvironmentVariable(name.to_string()))
}

fn headers_from_credential(
    config: Option<&McpCredentialConfig>,
    credential: Credential,
) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    match (config, credential) {
        (Some(McpCredentialConfig::ApiKey { header, .. }), Credential::ApiKey { value, .. }) => {
            headers.insert(header.clone(), value);
        }
        (_, Credential::ApiKey { header, value }) => {
            headers.insert(header, value);
        }
        (_, Credential::Bearer(token)) => {
            headers.insert("Authorization".to_string(), format!("Bearer {token}"));
        }
        (_, Credential::OAuth { access_token, .. }) => {
            headers.insert(
                "Authorization".to_string(),
                format!("Bearer {access_token}"),
            );
        }
    }
    headers
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use moa_core::{Credential, CredentialVault, McpCredentialConfig, McpServerConfig, SessionId};
    use tokio::time::sleep;
    use uuid::Uuid;

    use super::{EnvironmentCredentialVault, MCPCredentialProxy};

    struct MockVault {
        values: HashMap<(String, String), Credential>,
    }

    #[async_trait]
    impl CredentialVault for MockVault {
        async fn get(&self, service: &str, scope: &str) -> moa_core::Result<Credential> {
            self.values
                .get(&(service.to_string(), scope.to_string()))
                .cloned()
                .ok_or_else(|| moa_core::MoaError::StorageError("missing credential".to_string()))
        }

        async fn set(
            &self,
            _service: &str,
            _scope: &str,
            _cred: Credential,
        ) -> moa_core::Result<()> {
            Ok(())
        }

        async fn delete(&self, _service: &str, _scope: &str) -> moa_core::Result<()> {
            Ok(())
        }

        async fn list(&self, _scope: &str) -> moa_core::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn proxy_creates_tokens_and_injects_headers() {
        // Pins: MCP proxy tokens are opaque credentials for one call only.
        let vault: Arc<dyn CredentialVault> = Arc::new(MockVault {
            values: HashMap::from([(
                ("github".to_string(), "github".to_string()),
                Credential::Bearer("secret-token".to_string()),
            )]),
        });
        let proxy = MCPCredentialProxy::new(vault);
        let token = proxy
            .create_session_token(&SessionId::new(), "github", "github")
            .await
            .unwrap();

        let headers = proxy
            .enrich_headers(
                &token,
                Some(&McpCredentialConfig::Bearer {
                    token_env: "GITHUB_TOKEN".to_string(),
                }),
            )
            .await
            .unwrap();

        assert_eq!(
            headers.get("Authorization"),
            Some(&"Bearer secret-token".to_string())
        );
        assert!(!token.as_str().contains("secret-token"));
    }

    #[tokio::test]
    async fn proxy_grants_are_single_use() {
        // Pins: a process-local MCP credential grant cannot be exposed across requests.
        let vault: Arc<dyn CredentialVault> = Arc::new(MockVault {
            values: HashMap::from([(
                ("github".to_string(), "github".to_string()),
                Credential::Bearer("secret-token".to_string()),
            )]),
        });
        let proxy = MCPCredentialProxy::new(vault);
        let token = proxy
            .create_session_token(&SessionId::new(), "github", "github")
            .await
            .expect("test setup should create an MCP proxy token");

        proxy
            .enrich_headers(&token, None)
            .await
            .expect("first proxy enrichment should consume the grant");

        let error = proxy
            .enrich_headers(&token, None)
            .await
            .expect_err("second proxy enrichment should reject the consumed grant");

        assert!(
            matches!(error, moa_core::MoaError::PermissionDenied(ref message) if message.contains("unknown or expired MCP proxy token"))
        );
        assert!(
            !error.to_string().contains(token.as_str()),
            "proxy errors must not echo the full opaque grant"
        );
    }

    #[tokio::test]
    async fn proxy_token_is_rejected_after_ttl_expiry() {
        // Pins: a session token past its TTL is rejected as unknown-or-expired, never silently honored.
        let vault: Arc<dyn CredentialVault> = Arc::new(MockVault {
            values: HashMap::from([(
                ("github".to_string(), "github".to_string()),
                Credential::Bearer("secret-token".to_string()),
            )]),
        });
        let proxy = MCPCredentialProxy::new(vault).with_token_ttl(Duration::from_millis(1));
        let token = proxy
            .create_session_token(&SessionId::new(), "github", "github")
            .await
            .expect("test setup should create an MCP proxy token");

        sleep(Duration::from_millis(25)).await;

        let error = proxy
            .enrich_headers(&token, None)
            .await
            .expect_err("an expired proxy token must not resolve credentials");
        assert!(
            matches!(error, moa_core::MoaError::PermissionDenied(ref message) if message.contains("unknown or expired MCP proxy token"))
        );
        assert!(
            !error.to_string().contains(token.as_str()),
            "expired-token errors must not echo the full opaque grant"
        );
    }

    #[tokio::test]
    async fn revoked_proxy_token_is_rejected() {
        // Pins: revoking a session token before use prevents any later credential resolution.
        let vault: Arc<dyn CredentialVault> = Arc::new(MockVault {
            values: HashMap::from([(
                ("github".to_string(), "github".to_string()),
                Credential::Bearer("secret-token".to_string()),
            )]),
        });
        let proxy = MCPCredentialProxy::new(vault);
        let token = proxy
            .create_session_token(&SessionId::new(), "github", "github")
            .await
            .expect("test setup should create an MCP proxy token");

        proxy.revoke_session_token(&token).await;

        let error = proxy
            .enrich_headers(&token, None)
            .await
            .expect_err("a revoked proxy token must not resolve credentials");
        assert!(
            matches!(error, moa_core::MoaError::PermissionDenied(ref message) if message.contains("unknown or expired MCP proxy token"))
        );
        assert!(
            !error.to_string().contains(token.as_str()),
            "revoked-token errors must not echo the full opaque grant"
        );
    }

    #[tokio::test]
    async fn enrich_headers_shapes_api_key_and_oauth_credentials() {
        // Pins: ApiKey uses the configured custom header; OAuth maps to an Authorization Bearer header.
        let vault: Arc<dyn CredentialVault> = Arc::new(MockVault {
            values: HashMap::from([
                (
                    ("api".to_string(), "api".to_string()),
                    Credential::ApiKey {
                        header: "X-Credential-Header".to_string(),
                        value: "api-secret".to_string(),
                    },
                ),
                (
                    ("oauth".to_string(), "oauth".to_string()),
                    Credential::OAuth {
                        access_token: "oauth-secret".to_string(),
                        refresh_token: None,
                        expires_at: None,
                    },
                ),
            ]),
        });
        let proxy = MCPCredentialProxy::new(vault);

        let api_token = proxy
            .create_session_token(&SessionId::new(), "api", "api")
            .await
            .expect("test setup should create an API key proxy token");
        let api_headers = proxy
            .enrich_headers(
                &api_token,
                Some(&McpCredentialConfig::ApiKey {
                    header: "X-Configured-Header".to_string(),
                    value_env: "UNUSED_AT_CALL_TIME".to_string(),
                }),
            )
            .await
            .expect("api key enrichment should resolve a custom-header credential");
        // The configured header wins over the stored credential header, and no Bearer leaks in.
        assert_eq!(
            api_headers.get("X-Configured-Header"),
            Some(&"api-secret".to_string())
        );
        assert!(!api_headers.contains_key("Authorization"));

        let oauth_token = proxy
            .create_session_token(&SessionId::new(), "oauth", "oauth")
            .await
            .expect("test setup should create an OAuth proxy token");
        let oauth_headers = proxy
            .enrich_headers(&oauth_token, None)
            .await
            .expect("oauth enrichment should resolve an Authorization header");
        assert_eq!(
            oauth_headers.get("Authorization"),
            Some(&"Bearer oauth-secret".to_string())
        );
    }

    #[tokio::test]
    async fn environment_vault_fails_closed_on_missing_env_var() {
        // Pins: a credentialed MCP server whose token env var is unset fails closed at load, never unauthenticated.
        let name = format!("MOA_TEST_MISSING_{}", Uuid::now_v7());
        unsafe { std::env::remove_var(&name) };

        let result = EnvironmentCredentialVault::from_mcp_servers(&[McpServerConfig {
            name: "custom".to_string(),
            credentials: Some(McpCredentialConfig::Bearer {
                token_env: name.clone(),
            }),
            ..McpServerConfig::default()
        }]);

        let Err(error) = result else {
            panic!("a missing credential env var must fail vault construction");
        };
        assert!(
            matches!(error, moa_core::MoaError::MissingEnvironmentVariable(var) if var == name)
        );
    }

    #[tokio::test]
    async fn environment_vault_rejects_unknown_service_scope() {
        // Pins: looking up an unconfigured service/scope is a typed error, not an empty credential.
        let vault = EnvironmentCredentialVault::from_mcp_servers(&[])
            .expect("an empty server list builds an empty vault");

        let error = vault
            .get("unknown-service", "unknown-scope")
            .await
            .expect_err("an unconfigured service/scope lookup must fail closed");

        assert!(
            matches!(error, moa_core::MoaError::MissingEnvironmentVariable(message) if message.contains("credential not configured for service unknown-service"))
        );
    }

    #[tokio::test]
    async fn environment_vault_loads_from_env_backed_server_config() {
        let name = format!("MOA_TEST_TOKEN_{}", Uuid::now_v7());
        unsafe { std::env::set_var(&name, "env-token") };

        let vault = EnvironmentCredentialVault::from_mcp_servers(&[McpServerConfig {
            name: "custom".to_string(),
            credentials: Some(McpCredentialConfig::Bearer {
                token_env: name.clone(),
            }),
            ..McpServerConfig::default()
        }])
        .unwrap();

        let credential = vault.get("custom", "custom").await.unwrap();
        assert_eq!(credential, Credential::Bearer("env-token".to_string()));

        unsafe { std::env::remove_var(name) };
    }
}

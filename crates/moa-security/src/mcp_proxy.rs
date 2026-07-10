//! Session-scoped credential resolution for MCP-backed tool calls.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{
    Credential, CredentialVault, McpCredentialConfig, McpServerConfig, MoaError, Result, SessionId,
    StoredCredentialMetadata,
};
use tokio::sync::RwLock;

/// MCP credential resolver that reads real credentials from a vault only at call time.
pub struct MCPCredentialProxy {
    vault: Arc<dyn CredentialVault>,
}

impl MCPCredentialProxy {
    /// Creates a new MCP credential resolver backed by `vault`.
    pub fn new(vault: Arc<dyn CredentialVault>) -> Self {
        Self { vault }
    }

    /// Resolves credential headers for one MCP call by reading the vault directly.
    ///
    /// This is trusted, in-process host-side resolution: `server` and `operation`
    /// select the vault credential and `config` shapes the injected headers.
    ///
    /// No proxy token is minted here. The previous design minted an opaque token
    /// and consumed it inside this same host function, so the token added cache,
    /// expiry, and allocation cost without ever crossing an isolation boundary.
    /// Reintroduce a single-use token returned from this call — bound to
    /// `session_id`, `server`, `operation`, an expiry, and one use — only when a
    /// real remote proxy boundary sits between this resolver and the MCP
    /// transport that consumes the credential.
    pub async fn enrich_headers(
        &self,
        session_id: &SessionId,
        server: &str,
        operation: &str,
        config: Option<&McpCredentialConfig>,
    ) -> Result<HashMap<String, String>> {
        let credential = self.vault.get(server, operation).await?;
        tracing::debug!(
            %session_id,
            server,
            operation,
            "resolved MCP credential headers from vault"
        );
        Ok(headers_from_credential(config, credential))
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

    async fn delete(&self, service: &str, scope: &str) -> Result<bool> {
        Ok(self
            .credentials
            .write()
            .await
            .remove(&(service.to_string(), scope.to_string()))
            .is_some())
    }

    async fn list(&self, service_prefix: &str) -> Result<Vec<StoredCredentialMetadata>> {
        let mut entries = self
            .credentials
            .read()
            .await
            .iter()
            .filter(|((service, _), _)| service.starts_with(service_prefix))
            .map(|((service, scope), credential)| StoredCredentialMetadata {
                service: service.clone(),
                scope: scope.clone(),
                kind: credential_kind(credential).to_string(),
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.service
                .cmp(&right.service)
                .then_with(|| left.scope.cmp(&right.scope))
        });
        Ok(entries)
    }
}

fn credential_kind(credential: &Credential) -> &'static str {
    match credential {
        Credential::Bearer(_) => "bearer",
        Credential::OAuth { .. } => "oauth",
        Credential::ApiKey { .. } => "api_key",
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

    use async_trait::async_trait;
    use moa_core::{
        Credential, CredentialVault, McpCredentialConfig, McpServerConfig, SessionId,
        StoredCredentialMetadata,
    };
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

        async fn delete(&self, _service: &str, _scope: &str) -> moa_core::Result<bool> {
            Ok(false)
        }

        async fn list(
            &self,
            _service_prefix: &str,
        ) -> moa_core::Result<Vec<StoredCredentialMetadata>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn enrich_headers_resolves_bearer_credential_from_vault() {
        // Pins: trusted host dispatch resolves the (server, operation) vault credential
        // directly and shapes it into an Authorization header, with no token indirection.
        let vault: Arc<dyn CredentialVault> = Arc::new(MockVault {
            values: HashMap::from([(
                ("github".to_string(), "github".to_string()),
                Credential::Bearer("secret-token".to_string()),
            )]),
        });
        let proxy = MCPCredentialProxy::new(vault);

        let headers = proxy
            .enrich_headers(
                &SessionId::new(),
                "github",
                "github",
                Some(&McpCredentialConfig::Bearer {
                    token_env: "GITHUB_TOKEN".to_string(),
                }),
            )
            .await
            .expect("bearer credential should resolve into an Authorization header");

        assert_eq!(
            headers.get("Authorization"),
            Some(&"Bearer secret-token".to_string())
        );
    }

    #[tokio::test]
    async fn enrich_headers_fails_closed_on_missing_credential() {
        // Pins: an unconfigured (server, operation) credential is a typed vault error,
        // never an empty header map that would send the MCP call unauthenticated.
        let vault: Arc<dyn CredentialVault> = Arc::new(
            EnvironmentCredentialVault::from_mcp_servers(&[])
                .expect("an empty server list builds an empty vault"),
        );
        let proxy = MCPCredentialProxy::new(vault);

        let error = proxy
            .enrich_headers(&SessionId::new(), "unknown-server", "unknown-server", None)
            .await
            .expect_err("a missing credential must fail closed, not return empty headers");

        assert!(
            matches!(error, moa_core::MoaError::MissingEnvironmentVariable(message) if message.contains("credential not configured for service unknown-server"))
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

        let api_headers = proxy
            .enrich_headers(
                &SessionId::new(),
                "api",
                "api",
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

        let oauth_headers = proxy
            .enrich_headers(&SessionId::new(), "oauth", "oauth", None)
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
    async fn environment_vault_lists_metadata_without_secret_and_delete_removes_entry() {
        // Pins: knowledge disconnect can enumerate and revoke managed vault refs without exposing token material.
        let vault = EnvironmentCredentialVault::from_mcp_servers(&[])
            .expect("an empty server list builds an empty vault");
        vault
            .set(
                "knowledge:nango",
                "tenant-a:account-1",
                Credential::Bearer("secret-token".to_string()),
            )
            .await
            .expect("setup should store a managed knowledge credential");
        vault
            .set(
                "messaging:postmark",
                "tenant-a",
                Credential::ApiKey {
                    header: "X-Api-Key".to_string(),
                    value: "postmark-secret".to_string(),
                },
            )
            .await
            .expect("setup should store an unrelated credential");

        let metadata = vault
            .list("knowledge:")
            .await
            .expect("metadata listing should succeed");
        assert_eq!(
            metadata,
            vec![StoredCredentialMetadata {
                service: "knowledge:nango".to_string(),
                scope: "tenant-a:account-1".to_string(),
                kind: "bearer".to_string(),
            }]
        );
        assert!(!format!("{metadata:?}").contains("secret-token"));
        assert!(!format!("{metadata:?}").contains("postmark-secret"));

        assert!(
            vault
                .delete("knowledge:nango", "tenant-a:account-1")
                .await
                .expect("delete should succeed for existing credential")
        );
        assert!(
            !vault
                .delete("knowledge:nango", "tenant-a:account-1")
                .await
                .expect("second delete should be idempotent")
        );
        assert!(
            vault
                .get("knowledge:nango", "tenant-a:account-1")
                .await
                .is_err(),
            "deleted credential must no longer resolve"
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

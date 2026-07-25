//! Behavior tests for MCP credential header resolution.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use moa_config::McpCredentialConfig;
use moa_core::traits::CredentialVault;
use moa_core::types::credentials::{
    CredentialContext, CredentialError, CredentialIdentity, CredentialOperation,
    CredentialPrincipal, CredentialRef, CredentialSource, CredentialVersion, RedactedSecret,
};
use moa_core::types::identifiers::{SessionId, TenantId};
use secrecy::SecretString;
use uuid::Uuid;

use super::MCPCredentialProxy;

/// Vault stub that returns one secret and counts resolutions.
struct StubVault {
    secret: String,
    resolves: AtomicUsize,
    fail: bool,
}

impl StubVault {
    fn new(secret: &str) -> Self {
        Self {
            secret: secret.to_string(),
            resolves: AtomicUsize::new(0),
            fail: false,
        }
    }

    fn failing() -> Self {
        Self {
            secret: String::new(),
            resolves: AtomicUsize::new(0),
            fail: true,
        }
    }
}

#[async_trait]
impl CredentialVault for StubVault {
    async fn create(
        &self,
        _identity: CredentialIdentity,
        _material: SecretString,
        _ctx: &CredentialContext,
    ) -> Result<CredentialVersion, CredentialError> {
        Err(CredentialError::Unauthorized)
    }

    async fn resolve(
        &self,
        _source: &CredentialSource,
        _ctx: &CredentialContext,
    ) -> Result<RedactedSecret, CredentialError> {
        self.resolves.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Err(CredentialError::Revoked);
        }
        Ok(RedactedSecret::new(self.secret.clone()))
    }

    async fn describe(
        &self,
        _reference: CredentialRef,
        _ctx: &CredentialContext,
    ) -> Result<CredentialVersion, CredentialError> {
        Err(CredentialError::NotFound)
    }

    async fn rotate(
        &self,
        _current: CredentialRef,
        _material: SecretString,
        _ctx: &CredentialContext,
    ) -> Result<CredentialVersion, CredentialError> {
        Err(CredentialError::Unauthorized)
    }

    async fn revoke(
        &self,
        _reference: CredentialRef,
        _ctx: &CredentialContext,
    ) -> Result<(), CredentialError> {
        Err(CredentialError::Unauthorized)
    }

    async fn delete_connection(
        &self,
        _connection_uid: Uuid,
        _ctx: &CredentialContext,
    ) -> Result<u64, CredentialError> {
        Err(CredentialError::Unauthorized)
    }

    async fn purge_tenant(
        &self,
        _limit: u32,
        _ctx: &CredentialContext,
    ) -> Result<u64, CredentialError> {
        Err(CredentialError::Unauthorized)
    }
}

fn resolve_context() -> CredentialContext {
    CredentialContext {
        tenant_id: TenantId::from(Uuid::from_u128(0x2301)),
        principal: CredentialPrincipal::Caller {
            identity_id: Uuid::from_u128(0x2302),
            delegated_by: None,
        },
        operation: CredentialOperation::Resolve,
        operation_id: "op-mcp-resolve".to_string(),
        request_hash: "hash-mcp-resolve".to_string(),
    }
}

fn source() -> CredentialSource {
    CredentialSource::TenantConnection {
        reference: CredentialRef::from_uuid(Uuid::from_u128(0x2303)),
    }
}

async fn headers_for(
    vault: StubVault,
    config: Option<McpCredentialConfig>,
) -> moa_core::error::Result<HashMap<String, String>> {
    let proxy = MCPCredentialProxy::new(Arc::new(vault));
    proxy
        .enrich_headers(
            &SessionId::new(),
            &source(),
            config.as_ref(),
            &resolve_context(),
        )
        .await
}

#[tokio::test]
async fn bearer_config_renders_an_authorization_header() {
    // Pins: a bearer-configured MCP server receives the resolved secret as a
    // standard Authorization header and nothing else.
    let headers = headers_for(
        StubVault::new("mcp-bearer-secret"),
        Some(McpCredentialConfig::Bearer {
            token_env: "IGNORED_NOW".to_string(),
        }),
    )
    .await
    .expect("bearer headers resolve");

    assert_eq!(
        headers.get("Authorization").map(String::as_str),
        Some("Bearer mcp-bearer-secret")
    );
    assert_eq!(headers.len(), 1);
}

#[tokio::test]
async fn api_key_config_renders_its_configured_header_name() {
    // Pins: an API-key server receives the secret under the operator-configured
    // header name rather than a hardcoded Authorization header.
    let headers = headers_for(
        StubVault::new("mcp-api-key-secret"),
        Some(McpCredentialConfig::ApiKey {
            header: "X-Api-Key".to_string(),
            value_env: "IGNORED_NOW".to_string(),
        }),
    )
    .await
    .expect("api key headers resolve");

    assert_eq!(
        headers.get("X-Api-Key").map(String::as_str),
        Some("mcp-api-key-secret")
    );
    assert!(!headers.contains_key("Authorization"));
}

#[tokio::test]
async fn a_server_without_credential_config_receives_no_headers() {
    // Pins: an unauthenticated MCP server gets no injected credential headers
    // even though a source was supplied.
    let headers = headers_for(StubVault::new("unused-secret"), None)
        .await
        .expect("headers resolve");

    assert!(headers.is_empty());
}

#[tokio::test]
async fn a_vault_denial_fails_the_call_without_leaking_the_reason_body() {
    // Pins: an unusable credential fails the outbound call closed, and the error
    // surface carries only the typed vault reason — never material.
    let error = headers_for(
        StubVault::failing(),
        Some(McpCredentialConfig::Bearer {
            token_env: "IGNORED_NOW".to_string(),
        }),
    )
    .await
    .expect_err("a revoked credential must fail the call");

    let rendered = error.to_string();
    assert!(
        rendered.contains("revoked"),
        "expected the typed vault reason, got: {rendered}"
    );
}

#[tokio::test]
async fn each_call_resolves_from_the_vault_rather_than_caching() {
    // Pins: credentials are read at call time, so a rotation or revocation takes
    // effect on the very next MCP call instead of being served from a cache.
    let vault = Arc::new(StubVault::new("per-call-secret"));
    let proxy = MCPCredentialProxy::new(vault.clone());
    let config = McpCredentialConfig::Bearer {
        token_env: "IGNORED_NOW".to_string(),
    };

    for _ in 0..3 {
        proxy
            .enrich_headers(
                &SessionId::new(),
                &source(),
                Some(&config),
                &resolve_context(),
            )
            .await
            .expect("headers resolve");
    }

    assert_eq!(vault.resolves.load(Ordering::SeqCst), 3);
}

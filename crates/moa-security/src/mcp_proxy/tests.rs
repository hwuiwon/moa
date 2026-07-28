//! Behavior tests for MCP credential header resolution.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use moa_config::{
    McpCredentialConfig, McpServerConfig, McpServerCredentialScope, McpTransportConfig,
};
use moa_core::traits::CredentialVault;
use moa_core::types::credentials::{
    CredentialContext, CredentialError, CredentialIdentity, CredentialKind, CredentialOperation,
    CredentialPrincipal, CredentialRef, CredentialSource, CredentialVersion, RedactedSecret,
};
use moa_core::types::identifiers::{SessionId, TenantId};
use secrecy::SecretString;
use uuid::Uuid;

use super::{MCPCredentialProxy, McpDeploymentCredentials};

const TENANT: u128 = 0x2301;
const CONNECTION: u128 = 0x2304;
const REFERENCE: u128 = 0x2303;

/// Vault stub that returns one secret, reports one stored identity, and counts
/// how far a resolution got.
struct StubVault {
    secret: String,
    stored_identity: CredentialIdentity,
    describes: AtomicUsize,
    resolves: AtomicUsize,
    fail: bool,
}

impl StubVault {
    fn new(secret: &str) -> Self {
        Self {
            secret: secret.to_string(),
            stored_identity: expected_identity(),
            describes: AtomicUsize::new(0),
            resolves: AtomicUsize::new(0),
            fail: false,
        }
    }

    fn failing() -> Self {
        Self {
            fail: true,
            ..Self::new("")
        }
    }

    /// Stores the credential under a different identity than the binding claims.
    fn stored_under(mut self, identity: CredentialIdentity) -> Self {
        self.stored_identity = identity;
        self
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
        reference: CredentialRef,
        _ctx: &CredentialContext,
    ) -> Result<CredentialVersion, CredentialError> {
        self.describes.fetch_add(1, Ordering::SeqCst);
        Ok(CredentialVersion {
            reference,
            identity: self.stored_identity,
            version: 1,
            active: true,
            revoked: false,
            created_at: chrono::Utc::now(),
        })
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
        tenant_id: TenantId::from(Uuid::from_u128(TENANT)),
        principal: CredentialPrincipal::Caller {
            identity_id: Uuid::from_u128(0x2302),
            delegated_by: None,
        },
        operation: CredentialOperation::Resolve,
        operation_id: "op-mcp-resolve".to_string(),
        request_hash: "hash-mcp-resolve".to_string(),
    }
}

fn expected_identity() -> CredentialIdentity {
    CredentialIdentity {
        tenant_id: TenantId::from(Uuid::from_u128(TENANT)),
        connection_uid: Uuid::from_u128(CONNECTION),
        kind: CredentialKind::McpBearer,
    }
}

fn reference() -> CredentialRef {
    CredentialRef::from_uuid(Uuid::from_u128(REFERENCE))
}

async fn headers_for(
    vault: Arc<StubVault>,
    config: McpCredentialConfig,
) -> moa_core::error::Result<HashMap<String, String>> {
    let proxy = MCPCredentialProxy::new(vault);
    proxy
        .tenant_headers(
            &SessionId::new(),
            expected_identity(),
            reference(),
            &config,
            &resolve_context(),
        )
        .await
}

fn server(
    name: &str,
    credential_scope: McpServerCredentialScope,
    credentials: Option<McpCredentialConfig>,
) -> McpServerConfig {
    McpServerConfig {
        name: name.to_string(),
        transport: McpTransportConfig::Http,
        url: Some("http://127.0.0.1:1".to_string()),
        credential_scope,
        credentials,
        trust_tool_annotations: false,
        allowed_data_classes: Vec::new(),
    }
}

#[tokio::test]
async fn tenant_bearer_config_renders_an_authorization_header() {
    // Pins: a tenant-owned bearer server receives the tenant's resolved secret as
    // a standard Authorization header and nothing else.
    let headers = headers_for(
        Arc::new(StubVault::new("mcp-bearer-secret")),
        McpCredentialConfig::TenantBearer,
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
async fn tenant_api_key_config_renders_its_configured_header_name() {
    // Pins: a tenant-owned API-key server receives the secret under the
    // operator-configured header name rather than a hardcoded Authorization
    // header.
    let headers = headers_for(
        Arc::new(StubVault::new("mcp-api-key-secret")),
        McpCredentialConfig::TenantApiKey {
            header: "X-Api-Key".to_string(),
        },
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
async fn a_vault_denial_fails_the_call_without_leaking_the_reason_body() {
    // Pins: an unusable credential fails the outbound call closed, and the error
    // surface carries only the typed vault reason — never material.
    let error = headers_for(
        Arc::new(StubVault::failing()),
        McpCredentialConfig::TenantBearer,
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

    for _ in 0..3 {
        proxy
            .tenant_headers(
                &SessionId::new(),
                expected_identity(),
                reference(),
                &McpCredentialConfig::TenantBearer,
                &resolve_context(),
            )
            .await
            .expect("headers resolve");
    }

    assert_eq!(vault.resolves.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn a_reference_stored_under_another_connection_is_refused_before_resolution() {
    // Pins: a binding that names a credential belonging to a different connection
    // is refused on the stored identity, and no material is ever opened.
    let vault = Arc::new(StubVault::new("other-connection-secret").stored_under(
        CredentialIdentity {
            connection_uid: Uuid::from_u128(0xdead_beef),
            ..expected_identity()
        },
    ));

    let error = MCPCredentialProxy::new(vault.clone())
        .tenant_headers(
            &SessionId::new(),
            expected_identity(),
            reference(),
            &McpCredentialConfig::TenantBearer,
            &resolve_context(),
        )
        .await
        .expect_err("a reference on another connection must be refused");

    assert!(
        error.to_string().contains("expected connection"),
        "expected the typed wrong-connection reason, got: {error}"
    );
    assert_eq!(
        vault.resolves.load(Ordering::SeqCst),
        0,
        "a drifted reference must never be opened"
    );
}

#[tokio::test]
async fn a_reference_stored_under_another_kind_is_refused_before_resolution() {
    // Pins: a credential of a different material kind cannot be presented to an
    // MCP server just because a binding points at it.
    let vault = Arc::new(
        StubVault::new("provider-key-secret").stored_under(CredentialIdentity {
            kind: CredentialKind::ProviderApiKey,
            ..expected_identity()
        }),
    );

    let error = MCPCredentialProxy::new(vault.clone())
        .tenant_headers(
            &SessionId::new(),
            expected_identity(),
            reference(),
            &McpCredentialConfig::TenantBearer,
            &resolve_context(),
        )
        .await
        .expect_err("a reference of another kind must be refused");

    assert!(
        error.to_string().contains("requested kind"),
        "expected the typed wrong-kind reason, got: {error}"
    );
    assert_eq!(vault.resolves.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_binding_for_another_tenant_is_refused_before_the_vault_is_touched() {
    // Pins: a binding whose tenant disagrees with the resolving context is denied
    // before any vault call, so a cross-tenant binding cannot even probe for the
    // existence of another tenant's credential.
    let vault = Arc::new(StubVault::new("other-tenant-secret"));

    let error = MCPCredentialProxy::new(vault.clone())
        .tenant_headers(
            &SessionId::new(),
            CredentialIdentity {
                tenant_id: TenantId::new(),
                ..expected_identity()
            },
            reference(),
            &McpCredentialConfig::TenantBearer,
            &resolve_context(),
        )
        .await
        .expect_err("a cross-tenant binding must be refused");

    assert!(
        error
            .to_string()
            .contains("does not belong to the resolving tenant"),
        "expected the cross-tenant denial, got: {error}"
    );
    assert_eq!(vault.describes.load(Ordering::SeqCst), 0);
    assert_eq!(vault.resolves.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn tenant_resolution_without_a_vault_fails_closed() {
    // Pins: a deployment-only resolver cannot serve a tenant-owned call at all.
    // The absent vault is not a fallback to the operator credential.
    let proxy = MCPCredentialProxy::deployment_only(McpDeploymentCredentials::default());

    assert!(!proxy.serves_tenant_owned());
    let error = proxy
        .tenant_headers(
            &SessionId::new(),
            expected_identity(),
            reference(),
            &McpCredentialConfig::TenantBearer,
            &resolve_context(),
        )
        .await
        .expect_err("tenant resolution without a vault must fail");

    assert!(
        error
            .to_string()
            .contains("requires an attached credential"),
        "expected the missing-vault configuration error, got: {error}"
    );
}

#[test]
fn tenant_owned_server_rejects_a_deployment_environment_selector() {
    // Pins: environment material stays reachable only from explicitly
    // deployment-owned connectors — a tenant-owned server that names an
    // environment variable is refused when the deployment set is built.
    let error = McpDeploymentCredentials::from_mcp_servers(&[server(
        "tenant-search",
        McpServerCredentialScope::TenantOwned,
        Some(McpCredentialConfig::Bearer {
            token_env: "MOA_TEST_SHOULD_NEVER_BE_READ".to_string(),
        }),
    )])
    .expect_err("a tenant-owned deployment selector must be rejected");

    let rendered = error.to_string();
    assert!(rendered.contains("tenant-search"), "got: {rendered}");
    assert!(
        rendered.contains("must not name a deployment environment variable"),
        "got: {rendered}"
    );
}

#[test]
fn tenant_owned_server_requires_a_header_shape() {
    // Pins: a tenant-owned server with no credential configuration has no way to
    // present its tenant's secret, which is a configuration error rather than an
    // unauthenticated call.
    let error = McpDeploymentCredentials::from_mcp_servers(&[server(
        "tenant-search",
        McpServerCredentialScope::TenantOwned,
        None,
    )])
    .expect_err("a tenant-owned server without a header shape must be rejected");

    assert!(
        error.to_string().contains("must declare the header shape"),
        "got: {error}"
    );
}

#[test]
fn deployment_owned_server_rejects_a_tenant_header_shape() {
    // Pins: the ownership branches cannot be crossed from the other direction
    // either — a deployment-owned server must name deployment material.
    let error = McpDeploymentCredentials::from_mcp_servers(&[server(
        "operator-search",
        McpServerCredentialScope::DeploymentOwned,
        Some(McpCredentialConfig::TenantBearer),
    )])
    .expect_err("a deployment-owned tenant header shape must be rejected");

    assert!(
        error
            .to_string()
            .contains("must name a deployment environment variable"),
        "got: {error}"
    );
}

#[test]
fn a_tenant_owned_server_contributes_no_deployment_credential() {
    // Pins: building the deployment credential set for a valid tenant-owned
    // server registers nothing, so no deployment credential exists that a tenant
    // dispatch could accidentally be served from.
    let deployment = McpDeploymentCredentials::from_mcp_servers(&[
        server(
            "tenant-search",
            McpServerCredentialScope::TenantOwned,
            Some(McpCredentialConfig::TenantBearer),
        ),
        server(
            "public-docs",
            McpServerCredentialScope::DeploymentOwned,
            None,
        ),
    ])
    .expect("valid ownership combinations build");

    assert!(!deployment.contains("tenant-search"));
    assert!(!deployment.contains("public-docs"));
}

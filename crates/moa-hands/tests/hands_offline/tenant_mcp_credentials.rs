//! Tenant-owned MCP credential scoping through the real tool-router dispatch path.
//!
//! Every test here drives `ToolRouter::execute_authorized`, the same entry point
//! the durable tool executor calls, so nothing is proven through a test-only
//! dispatch shortcut. The binding owner, credential vault, and tenant-operator
//! authorizer are in-memory doubles; their Postgres counterparts, row-level
//! security, and real credential versions are covered by the `hands_db` lane.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use moa_config::McpCredentialConfig;
use moa_config::McpServerConfig;
use moa_config::McpServerCredentialScope;
use moa_config::McpTransportConfig;
use moa_config::MoaConfig;
use moa_core::error::{MoaError, Result};
use moa_core::traits::CredentialVault;
use moa_core::types::credentials::{
    CredentialContext, CredentialError, CredentialIdentity, CredentialKind, CredentialRef,
    CredentialSource, CredentialVersion, RedactedSecret,
};
use moa_core::types::identifiers::TenantId;
use moa_core::types::identifiers::ToolCallId;
use moa_core::{
    traits::Identity, types::completion::ToolInvocation, types::identifiers::ModelId,
    types::session::SessionMeta,
};
use moa_hands::ToolRouter;
use moa_hands::core::mcp_connections::{
    TenantMcpAuthorizer, TenantMcpBindingStatus, TenantMcpConnectionBinding,
    TenantMcpConnectionBindingStore, TenantMcpCredentialOwners,
};
use serde_json::json;
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

use super::mcp_router::{mcp_egress_guard, opt_into_development_local_hands};

const SERVER: &str = "tenant-search";
const OPERATION: &str = "search_documents";

/// One recorded outbound `tools/call`: its credential header and its JSON-RPC
/// body, so a test can assert exactly where material is allowed to appear.
#[derive(Clone)]
struct RecordedCall {
    authorization: String,
    body: String,
}

/// Fake MCP server that answers the handshake and records every `tools/call`.
struct RecordingMcpServer {
    url: String,
    calls: Arc<Mutex<Vec<RecordedCall>>>,
}

async fn spawn_recording_mcp_server() -> RecordingMcpServer {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake MCP server");
    let addr = listener.local_addr().expect("fake MCP server address");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&calls);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = vec![0_u8; 16384];
            let bytes = match socket.read(&mut buffer).await {
                Ok(0) | Err(_) => continue,
                Ok(read) => read,
            };
            let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
            let method = request
                .split_once("\r\n\r\n")
                .and_then(|(_, body)| serde_json::from_str::<serde_json::Value>(body).ok())
                .and_then(|value| {
                    value
                        .get("method")
                        .and_then(|method| method.as_str())
                        .map(str::to_string)
                });
            let body = match method.as_deref() {
                Some("initialize") => r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{}}}"#.to_string(),
                Some("tools/list") => format!(
                    r#"{{"jsonrpc":"2.0","id":2,"result":{{"tools":[{{"name":"{OPERATION}","description":"Search","inputSchema":{{"type":"object","properties":{{}},"additionalProperties":false}}}}]}}}}"#
                ),
                Some("tools/call") => {
                    let authorization = request
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("authorization")
                                .then(|| value.trim().to_string())
                        })
                        .unwrap_or_default();
                    let body = request
                        .split_once("\r\n\r\n")
                        .map(|(_, body)| body.to_string())
                        .unwrap_or_default();
                    recorded
                        .lock()
                        .expect("record outbound call")
                        .push(RecordedCall { authorization, body });
                    r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"pong"}]}}"#.to_string()
                }
                _ => "{}".to_string(),
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    RecordingMcpServer {
        url: format!("http://{addr}"),
        calls,
    }
}

impl RecordingMcpServer {
    fn outbound_calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().expect("read recorded calls").clone()
    }

    fn outbound_authorizations(&self) -> Vec<String> {
        self.outbound_calls()
            .into_iter()
            .map(|call| call.authorization)
            .collect()
    }
}

/// In-memory binding owner that counts reads so a test can prove authorization
/// ran before the first one.
#[derive(Default)]
struct StubBindings {
    rows: Mutex<Vec<TenantMcpConnectionBinding>>,
    reads: AtomicUsize,
}

impl StubBindings {
    fn with(bindings: Vec<TenantMcpConnectionBinding>) -> Self {
        Self {
            rows: Mutex::new(bindings),
            reads: AtomicUsize::new(0),
        }
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl TenantMcpConnectionBindingStore for StubBindings {
    async fn binding_for_server(
        &self,
        tenant_id: TenantId,
        server_name: &str,
    ) -> Result<Option<TenantMcpConnectionBinding>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .rows
            .lock()
            .expect("read stub bindings")
            .iter()
            .find(|binding| binding.tenant_id == tenant_id && binding.server_name == server_name)
            .cloned())
    }

    async fn upsert_binding(&self, binding: &TenantMcpConnectionBinding) -> Result<()> {
        let mut rows = self.rows.lock().expect("write stub bindings");
        rows.retain(|row| {
            row.tenant_id != binding.tenant_id
                || row.connection_uid != binding.connection_uid
                || row.server_name != binding.server_name
        });
        rows.push(binding.clone());
        Ok(())
    }
}

/// Binding owner that answers every lookup with one fixed row, whatever was
/// asked for. Used to prove the router does not trust the owner's answer.
struct LyingBindings(TenantMcpConnectionBinding);

#[async_trait]
impl TenantMcpConnectionBindingStore for LyingBindings {
    async fn binding_for_server(
        &self,
        _tenant_id: TenantId,
        _server_name: &str,
    ) -> Result<Option<TenantMcpConnectionBinding>> {
        Ok(Some(self.0.clone()))
    }

    async fn upsert_binding(&self, _binding: &TenantMcpConnectionBinding) -> Result<()> {
        Ok(())
    }
}

/// In-memory credential owner keyed by reference.
#[derive(Default)]
struct StubVault {
    secrets: Mutex<HashMap<Uuid, String>>,
    identities: Mutex<HashMap<Uuid, CredentialIdentity>>,
    stale: Mutex<Vec<Uuid>>,
}

impl StubVault {
    fn with_credential(
        self,
        reference: CredentialRef,
        identity: CredentialIdentity,
        secret: &str,
    ) -> Self {
        self.secrets
            .lock()
            .expect("seed stub secret")
            .insert(reference.as_uuid(), secret.to_string());
        self.identities
            .lock()
            .expect("seed stub identity")
            .insert(reference.as_uuid(), identity);
        self
    }

    fn superseded(self, reference: CredentialRef) -> Self {
        self.stale
            .lock()
            .expect("mark stub version stale")
            .push(reference.as_uuid());
        self
    }
}

#[async_trait]
impl CredentialVault for StubVault {
    async fn create(
        &self,
        _identity: CredentialIdentity,
        _material: secrecy::SecretString,
        _ctx: &CredentialContext,
    ) -> std::result::Result<CredentialVersion, CredentialError> {
        Err(CredentialError::Unauthorized)
    }

    async fn resolve(
        &self,
        source: &CredentialSource,
        _ctx: &CredentialContext,
    ) -> std::result::Result<RedactedSecret, CredentialError> {
        let CredentialSource::TenantConnection { reference } = source else {
            return Err(CredentialError::Unauthorized);
        };
        if self
            .stale
            .lock()
            .expect("read stub stale set")
            .contains(&reference.as_uuid())
        {
            return Err(CredentialError::StaleVersion);
        }
        self.secrets
            .lock()
            .expect("read stub secrets")
            .get(&reference.as_uuid())
            .map(|secret| RedactedSecret::new(secret.clone()))
            .ok_or(CredentialError::NotFound)
    }

    async fn describe(
        &self,
        reference: CredentialRef,
        _ctx: &CredentialContext,
    ) -> std::result::Result<CredentialVersion, CredentialError> {
        let identity = *self
            .identities
            .lock()
            .expect("read stub identities")
            .get(&reference.as_uuid())
            .ok_or(CredentialError::NotFound)?;
        Ok(CredentialVersion {
            reference,
            identity,
            version: 1,
            active: true,
            revoked: false,
            created_at: chrono::Utc::now(),
        })
    }

    async fn rotate(
        &self,
        _current: CredentialRef,
        _material: secrecy::SecretString,
        _ctx: &CredentialContext,
    ) -> std::result::Result<CredentialVersion, CredentialError> {
        Err(CredentialError::Unauthorized)
    }

    async fn revoke(
        &self,
        _reference: CredentialRef,
        _ctx: &CredentialContext,
    ) -> std::result::Result<(), CredentialError> {
        Err(CredentialError::Unauthorized)
    }

    async fn delete_connection(
        &self,
        _connection_uid: Uuid,
        _ctx: &CredentialContext,
    ) -> std::result::Result<u64, CredentialError> {
        Err(CredentialError::Unauthorized)
    }

    async fn purge_tenant(
        &self,
        _limit: u32,
        _ctx: &CredentialContext,
    ) -> std::result::Result<u64, CredentialError> {
        Err(CredentialError::Unauthorized)
    }
}

/// Authorizer that answers one fixed decision and counts its calls.
struct StubAuthorizer {
    allow: bool,
    calls: AtomicUsize,
}

impl StubAuthorizer {
    fn allowing() -> Self {
        Self {
            allow: true,
            calls: AtomicUsize::new(0),
        }
    }

    fn denying() -> Self {
        Self {
            allow: false,
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl TenantMcpAuthorizer for StubAuthorizer {
    async fn require_tenant_operator(
        &self,
        _identity: &Identity,
        tenant_id: TenantId,
    ) -> Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.allow {
            return Ok(());
        }
        Err(MoaError::PermissionDenied(format!(
            "identity is not a tenant operator on {tenant_id}"
        )))
    }
}

fn tenant(seed: u128) -> TenantId {
    TenantId::from(Uuid::from_u128(seed))
}

fn session_for(tenant_id: TenantId) -> SessionMeta {
    SessionMeta {
        tenant_id,
        model: ModelId::new("claude-sonnet-4-6"),
        ..SessionMeta::default()
    }
}

fn operator_of(tenant_id: TenantId) -> Identity {
    Identity {
        identity_type: moa_core::traits::IdentityType::Operator,
        id: Uuid::new_v4(),
        tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

fn binding(
    tenant_id: TenantId,
    reference: CredentialRef,
    connection_uid: Uuid,
) -> TenantMcpConnectionBinding {
    TenantMcpConnectionBinding {
        tenant_id,
        connection_uid,
        server_name: SERVER.to_string(),
        credential_ref: reference,
        status: TenantMcpBindingStatus::Active,
        allowed_operations: vec![OPERATION.to_string()],
    }
}

fn identity_of(binding: &TenantMcpConnectionBinding) -> CredentialIdentity {
    CredentialIdentity {
        tenant_id: binding.tenant_id,
        connection_uid: binding.connection_uid,
        kind: CredentialKind::McpBearer,
    }
}

fn tenant_owned_config(url: &str, sandbox_dir: &std::path::Path) -> MoaConfig {
    let mut config = MoaConfig::default();
    config.local.sandbox_dir = sandbox_dir.join("sandbox").display().to_string();
    opt_into_development_local_hands(&mut config);
    config.mcp_servers = vec![McpServerConfig {
        name: SERVER.to_string(),
        transport: McpTransportConfig::Http,
        url: Some(url.to_string()),
        credential_scope: McpServerCredentialScope::TenantOwned,
        credentials: Some(McpCredentialConfig::TenantBearer),
        trust_tool_annotations: false,
        allowed_data_classes: Vec::new(),
    }];
    config
}

fn invocation() -> ToolInvocation {
    ToolInvocation {
        id: None,
        name: OPERATION.to_string(),
        input: json!({}),
    }
}

async fn router_with(config: &MoaConfig, owners: TenantMcpCredentialOwners) -> ToolRouter {
    ToolRouter::from_config(config, Some(mcp_egress_guard()), None, Some(owners))
        .await
        .expect("tenant-owned router should build")
}

#[tokio::test]
async fn two_tenants_sharing_one_server_present_only_their_own_credential_offline() {
    // Pins: the least-privilege guarantee this task exists for — two tenants
    // invoking the same configured MCP server each present the credential their
    // own binding names, and neither can be served the other's.
    let server = spawn_recording_mcp_server().await;
    let dir = tempdir().expect("temp dir");
    let config = tenant_owned_config(&server.url, dir.path());

    let (first, second) = (tenant(0xa1), tenant(0xb2));
    let first_reference = CredentialRef::from_uuid(Uuid::from_u128(0xc1));
    let second_reference = CredentialRef::from_uuid(Uuid::from_u128(0xc2));
    let first_binding = binding(first, first_reference, Uuid::from_u128(0xd1));
    let second_binding = binding(second, second_reference, Uuid::from_u128(0xd2));
    let vault = StubVault::default()
        .with_credential(first_reference, identity_of(&first_binding), "first-secret")
        .with_credential(
            second_reference,
            identity_of(&second_binding),
            "second-secret",
        );
    let router = router_with(
        &config,
        TenantMcpCredentialOwners {
            vault: Arc::new(vault),
            bindings: Arc::new(StubBindings::with(vec![
                first_binding.clone(),
                second_binding.clone(),
            ])),
            authorizer: Arc::new(StubAuthorizer::allowing()),
        },
    )
    .await;

    for tenant_id in [first, second] {
        let secured = router
            .execute_authorized(
                &session_for(tenant_id),
                &operator_of(tenant_id),
                &invocation(),
                ToolCallId::new(),
                None,
            )
            .await
            .expect("a bound tenant should dispatch");
        let output = secured.safe_output;
        assert_eq!(output.to_text(), "pong");
    }

    assert_eq!(
        server.outbound_authorizations(),
        vec![
            "Bearer first-secret".to_string(),
            "Bearer second-secret".to_string()
        ],
        "each tenant must present exactly its own credential"
    );
}

#[tokio::test]
async fn the_resolved_secret_reaches_the_request_header_and_nothing_else_offline() {
    // Pins: the outbound header is the only place a tenant's material appears.
    // Everything the dispatch path can persist, serialize, or render — the
    // binding it resolved through, the invocation as it is serialized into
    // durable state, and the tool result handed back to the model — carries only
    // payload-safe metadata.
    let server = spawn_recording_mcp_server().await;
    let dir = tempdir().expect("temp dir");
    let config = tenant_owned_config(&server.url, dir.path());

    let tenant_id = tenant(0xae);
    let reference = CredentialRef::from_uuid(Uuid::from_u128(0xcd));
    let bound = binding(tenant_id, reference, Uuid::from_u128(0xdc));
    let router = router_with(
        &config,
        TenantMcpCredentialOwners {
            vault: Arc::new(StubVault::default().with_credential(
                reference,
                identity_of(&bound),
                "tenant-plaintext-secret",
            )),
            bindings: Arc::new(StubBindings::with(vec![bound.clone()])),
            authorizer: Arc::new(StubAuthorizer::allowing()),
        },
    )
    .await;

    let invocation = invocation();
    let secured_2 = router
        .execute_authorized(
            &session_for(tenant_id),
            &operator_of(tenant_id),
            &invocation,
            ToolCallId::new(),
            None,
        )
        .await
        .expect("a bound tenant should dispatch");
    let output = secured_2.safe_output;

    let calls = server.outbound_calls();
    assert_eq!(calls.len(), 1, "exactly one outbound call is made");
    assert_eq!(
        calls[0].authorization, "Bearer tenant-plaintext-secret",
        "the secret must reach the outbound credential header"
    );
    let serialized_output = serde_json::to_string(&output).expect("tool results are serialized");
    for (surface, rendered) in [
        ("outbound request body", calls[0].body.clone()),
        ("serialized tool result", serialized_output),
        ("model-visible tool text", output.to_text()),
        ("binding metadata", format!("{bound:?}")),
    ] {
        assert!(
            !rendered.contains("tenant-plaintext-secret"),
            "material must not appear in {surface}: {rendered}"
        );
    }
}

#[tokio::test]
async fn nothing_a_caller_supplies_can_change_the_credential_owner_offline() {
    // Pins the structural guarantee behind the derived credential scope: the
    // public dispatch entry point takes a session, a caller identity, a model-
    // authored invocation and a tool-call id, and none of them can name a
    // credential owner — the scope is read from the registered tool, which comes
    // from operator configuration. Here a model-authored payload that impersonates
    // deployment ownership in every way it can (tool arguments and provider
    // tool-use id) still takes the tenant path and is denied for the tenant
    // reason, never served an operator credential.
    let server = spawn_recording_mcp_server().await;
    let dir = tempdir().expect("temp dir");
    let config = tenant_owned_config(&server.url, dir.path());

    let tenant_id = tenant(0xb1);
    let bindings = Arc::new(StubBindings::default());
    let router = router_with(
        &config,
        TenantMcpCredentialOwners {
            vault: Arc::new(StubVault::default()),
            bindings: Arc::clone(&bindings) as Arc<dyn TenantMcpConnectionBindingStore>,
            authorizer: Arc::new(StubAuthorizer::allowing()),
        },
    )
    .await;

    let hostile = ToolInvocation {
        id: Some("credential_scope=deployment_owned_mcp".to_string()),
        name: OPERATION.to_string(),
        input: json!({}),
    };
    let error = router
        .execute_authorized(
            &session_for(tenant_id),
            &operator_of(tenant_id),
            &hostile,
            ToolCallId::new(),
            None,
        )
        .await
        .expect_err("a model-authored payload cannot select the deployment owner");

    assert!(
        matches!(error, MoaError::PermissionDenied(ref message) if message.contains("no MCP connection binding")),
        "the tenant path must run regardless of caller-supplied content, got: {error}"
    );
    assert_eq!(
        bindings.reads(),
        1,
        "the tenant binding path ran, so the scope came from the registry"
    );
    assert!(server.outbound_authorizations().is_empty());
}

#[tokio::test]
async fn missing_delegated_authorization_denies_before_any_binding_read_offline() {
    // Pins: authorization runs before the first binding read, so an unauthorized
    // caller cannot learn whether a tenant has a connection to a server, and no
    // outbound call is made.
    let server = spawn_recording_mcp_server().await;
    let dir = tempdir().expect("temp dir");
    let config = tenant_owned_config(&server.url, dir.path());

    let tenant_id = tenant(0xa3);
    let reference = CredentialRef::from_uuid(Uuid::from_u128(0xc3));
    let bound = binding(tenant_id, reference, Uuid::from_u128(0xd3));
    let bindings = Arc::new(StubBindings::with(vec![bound.clone()]));
    let authorizer = Arc::new(StubAuthorizer::denying());
    let router = router_with(
        &config,
        TenantMcpCredentialOwners {
            vault: Arc::new(StubVault::default().with_credential(
                reference,
                identity_of(&bound),
                "secret",
            )),
            bindings: Arc::clone(&bindings) as Arc<dyn TenantMcpConnectionBindingStore>,
            authorizer: Arc::clone(&authorizer) as Arc<dyn TenantMcpAuthorizer>,
        },
    )
    .await;

    let error = router
        .execute_authorized(
            &session_for(tenant_id),
            &operator_of(tenant_id),
            &invocation(),
            ToolCallId::new(),
            None,
        )
        .await
        .expect_err("an unauthorized caller must be denied");

    assert!(
        matches!(error, MoaError::PermissionDenied(ref message) if message.contains("tenant operator")),
        "expected the authorization denial, got: {error}"
    );
    assert_eq!(authorizer.calls(), 1);
    assert_eq!(
        bindings.reads(),
        0,
        "a denied caller must not reach the binding owner"
    );
    assert!(server.outbound_authorizations().is_empty());
}

#[tokio::test]
async fn a_caller_from_another_tenant_is_denied_before_authorization_runs_offline() {
    // Pins: the tenant a credential is resolved for is the session's, and a
    // caller identity belonging to a different tenant is refused outright rather
    // than authorized against the session's tenant.
    let server = spawn_recording_mcp_server().await;
    let dir = tempdir().expect("temp dir");
    let config = tenant_owned_config(&server.url, dir.path());

    let session_tenant = tenant(0xaf);
    let caller_tenant = tenant(0xb0);
    let reference = CredentialRef::from_uuid(Uuid::from_u128(0xce));
    let bound = binding(session_tenant, reference, Uuid::from_u128(0xdd));
    let authorizer = Arc::new(StubAuthorizer::allowing());
    let bindings = Arc::new(StubBindings::with(vec![bound.clone()]));
    let router = router_with(
        &config,
        TenantMcpCredentialOwners {
            vault: Arc::new(StubVault::default().with_credential(
                reference,
                identity_of(&bound),
                "secret",
            )),
            bindings: Arc::clone(&bindings) as Arc<dyn TenantMcpConnectionBindingStore>,
            authorizer: Arc::clone(&authorizer) as Arc<dyn TenantMcpAuthorizer>,
        },
    )
    .await;

    let error = router
        .execute_authorized(
            &session_for(session_tenant),
            &operator_of(caller_tenant),
            &invocation(),
            ToolCallId::new(),
            None,
        )
        .await
        .expect_err("a caller from another tenant must be denied");

    assert!(
        matches!(error, MoaError::PermissionDenied(ref message)
            if message.contains("does not match the session tenant")),
        "expected the caller/session tenant denial, got: {error}"
    );
    assert_eq!(
        authorizer.calls(),
        0,
        "a mismatched caller is refused before any authorization decision"
    );
    assert_eq!(bindings.reads(), 0);
    assert!(server.outbound_authorizations().is_empty());
}

#[tokio::test]
async fn a_tenant_without_a_binding_cannot_use_the_shared_server_offline() {
    // Pins: an unknown connection is a denial before dispatch, not a fallback to
    // whatever credential the deployment or another tenant has.
    let server = spawn_recording_mcp_server().await;
    let dir = tempdir().expect("temp dir");
    let config = tenant_owned_config(&server.url, dir.path());

    let bound_tenant = tenant(0xa4);
    let unbound_tenant = tenant(0xa5);
    let reference = CredentialRef::from_uuid(Uuid::from_u128(0xc4));
    let bound = binding(bound_tenant, reference, Uuid::from_u128(0xd4));
    let router = router_with(
        &config,
        TenantMcpCredentialOwners {
            vault: Arc::new(StubVault::default().with_credential(
                reference,
                identity_of(&bound),
                "bound-secret",
            )),
            bindings: Arc::new(StubBindings::with(vec![bound])),
            authorizer: Arc::new(StubAuthorizer::allowing()),
        },
    )
    .await;

    let error = router
        .execute_authorized(
            &session_for(unbound_tenant),
            &operator_of(unbound_tenant),
            &invocation(),
            ToolCallId::new(),
            None,
        )
        .await
        .expect_err("an unbound tenant must be denied");

    assert!(
        matches!(error, MoaError::PermissionDenied(ref message) if message.contains("no MCP connection binding")),
        "expected the unknown-connection denial, got: {error}"
    );
    assert!(
        server.outbound_authorizations().is_empty(),
        "a denied tenant must never reach the server"
    );
}

#[tokio::test]
async fn a_disabled_binding_denies_dispatch_offline() {
    // Pins: disabling a connection stops dispatch immediately and is reported as
    // disabled rather than as an unknown connection.
    let server = spawn_recording_mcp_server().await;
    let dir = tempdir().expect("temp dir");
    let config = tenant_owned_config(&server.url, dir.path());

    let tenant_id = tenant(0xa6);
    let reference = CredentialRef::from_uuid(Uuid::from_u128(0xc6));
    let mut disabled = binding(tenant_id, reference, Uuid::from_u128(0xd6));
    disabled.status = TenantMcpBindingStatus::Disabled;
    let router = router_with(
        &config,
        TenantMcpCredentialOwners {
            vault: Arc::new(StubVault::default().with_credential(
                reference,
                identity_of(&disabled),
                "secret",
            )),
            bindings: Arc::new(StubBindings::with(vec![disabled])),
            authorizer: Arc::new(StubAuthorizer::allowing()),
        },
    )
    .await;

    let error = router
        .execute_authorized(
            &session_for(tenant_id),
            &operator_of(tenant_id),
            &invocation(),
            ToolCallId::new(),
            None,
        )
        .await
        .expect_err("a disabled binding must be denied");

    assert!(
        matches!(error, MoaError::PermissionDenied(ref message) if message.contains("disabled")),
        "expected the disabled-binding denial, got: {error}"
    );
    assert!(server.outbound_authorizations().is_empty());
}

#[tokio::test]
async fn an_operation_outside_the_binding_allowlist_is_denied_offline() {
    // Pins: the binding's closed operation allowlist governs what the tenant's
    // credential may be used to do, not merely which server it reaches.
    let server = spawn_recording_mcp_server().await;
    let dir = tempdir().expect("temp dir");
    let config = tenant_owned_config(&server.url, dir.path());

    let tenant_id = tenant(0xa7);
    let reference = CredentialRef::from_uuid(Uuid::from_u128(0xc7));
    let mut restricted = binding(tenant_id, reference, Uuid::from_u128(0xd7));
    restricted.allowed_operations = vec!["list_documents".to_string()];
    let router = router_with(
        &config,
        TenantMcpCredentialOwners {
            vault: Arc::new(StubVault::default().with_credential(
                reference,
                identity_of(&restricted),
                "secret",
            )),
            bindings: Arc::new(StubBindings::with(vec![restricted])),
            authorizer: Arc::new(StubAuthorizer::allowing()),
        },
    )
    .await;

    let error = router
        .execute_authorized(
            &session_for(tenant_id),
            &operator_of(tenant_id),
            &invocation(),
            ToolCallId::new(),
            None,
        )
        .await
        .expect_err("an operation outside the allowlist must be denied");

    assert!(
        matches!(error, MoaError::PermissionDenied(ref message)
            if message.contains("does not permit operation") && message.contains(OPERATION)),
        "expected the forbidden-operation denial, got: {error}"
    );
    assert!(server.outbound_authorizations().is_empty());
}

#[tokio::test]
async fn a_superseded_credential_version_denies_dispatch_offline() {
    // Pins: a binding left pointing at a superseded version fails closed on the
    // vault's own staleness check instead of presenting an outdated secret.
    let server = spawn_recording_mcp_server().await;
    let dir = tempdir().expect("temp dir");
    let config = tenant_owned_config(&server.url, dir.path());

    let tenant_id = tenant(0xa8);
    let reference = CredentialRef::from_uuid(Uuid::from_u128(0xc8));
    let stale = binding(tenant_id, reference, Uuid::from_u128(0xd8));
    let router = router_with(
        &config,
        TenantMcpCredentialOwners {
            vault: Arc::new(
                StubVault::default()
                    .with_credential(reference, identity_of(&stale), "superseded-secret")
                    .superseded(reference),
            ),
            bindings: Arc::new(StubBindings::with(vec![stale])),
            authorizer: Arc::new(StubAuthorizer::allowing()),
        },
    )
    .await;

    let error = router
        .execute_authorized(
            &session_for(tenant_id),
            &operator_of(tenant_id),
            &invocation(),
            ToolCallId::new(),
            None,
        )
        .await
        .expect_err("a stale version must be denied");

    assert!(
        matches!(error, MoaError::PermissionDenied(ref message) if message.contains("stale")),
        "expected the stale-version denial, got: {error}"
    );
    assert!(server.outbound_authorizations().is_empty());
}

#[tokio::test]
async fn repointing_the_binding_takes_effect_on_the_next_call_without_restart_offline() {
    // Pins: rotation is served from durable state per call — repointing a binding
    // at the next credential version changes what the very next dispatch presents
    // on the same live router.
    let server = spawn_recording_mcp_server().await;
    let dir = tempdir().expect("temp dir");
    let config = tenant_owned_config(&server.url, dir.path());

    let tenant_id = tenant(0xa9);
    let connection_uid = Uuid::from_u128(0xd9);
    let first_reference = CredentialRef::from_uuid(Uuid::from_u128(0xc9));
    let next_reference = CredentialRef::from_uuid(Uuid::from_u128(0xca));
    let first_binding = binding(tenant_id, first_reference, connection_uid);
    let bindings = Arc::new(StubBindings::with(vec![first_binding.clone()]));
    let router = router_with(
        &config,
        TenantMcpCredentialOwners {
            vault: Arc::new(
                StubVault::default()
                    .with_credential(first_reference, identity_of(&first_binding), "version-one")
                    .with_credential(next_reference, identity_of(&first_binding), "version-two"),
            ),
            bindings: Arc::clone(&bindings) as Arc<dyn TenantMcpConnectionBindingStore>,
            authorizer: Arc::new(StubAuthorizer::allowing()),
        },
    )
    .await;

    router
        .execute_authorized(
            &session_for(tenant_id),
            &operator_of(tenant_id),
            &invocation(),
            ToolCallId::new(),
            None,
        )
        .await
        .expect("the first version dispatches");

    let rotated = TenantMcpConnectionBinding {
        credential_ref: next_reference,
        ..first_binding
    };
    bindings
        .upsert_binding(&rotated)
        .await
        .expect("repoint the binding");

    router
        .execute_authorized(
            &session_for(tenant_id),
            &operator_of(tenant_id),
            &invocation(),
            ToolCallId::new(),
            None,
        )
        .await
        .expect("the next version dispatches");

    assert_eq!(
        server.outbound_authorizations(),
        vec![
            "Bearer version-one".to_string(),
            "Bearer version-two".to_string()
        ],
        "the next call must use the repointed version without a restart"
    );
}

#[tokio::test]
async fn a_binding_for_a_different_server_is_refused_offline() {
    // Pins: the router does not trust the binding owner's answer — a row whose
    // server disagrees with the dispatched server is refused rather than used.
    let server = spawn_recording_mcp_server().await;
    let dir = tempdir().expect("temp dir");
    let config = tenant_owned_config(&server.url, dir.path());

    let tenant_id = tenant(0xaa);
    let reference = CredentialRef::from_uuid(Uuid::from_u128(0xcb));
    let mut drifted = binding(tenant_id, reference, Uuid::from_u128(0xda));
    drifted.server_name = "some-other-server".to_string();
    let router = router_with(
        &config,
        TenantMcpCredentialOwners {
            vault: Arc::new(StubVault::default().with_credential(
                reference,
                identity_of(&drifted),
                "secret",
            )),
            bindings: Arc::new(LyingBindings(drifted)),
            authorizer: Arc::new(StubAuthorizer::allowing()),
        },
    )
    .await;

    let error = router
        .execute_authorized(
            &session_for(tenant_id),
            &operator_of(tenant_id),
            &invocation(),
            ToolCallId::new(),
            None,
        )
        .await
        .expect_err("a binding for another server must be refused");

    assert!(
        matches!(error, MoaError::PermissionDenied(ref message)
            if message.contains("does not belong to this tenant and server")),
        "expected the server-drift denial, got: {error}"
    );
    assert!(server.outbound_authorizations().is_empty());
}

#[tokio::test]
async fn a_binding_for_another_tenant_is_refused_offline() {
    // Pins: a binding row belonging to another tenant cannot serve this session
    // even if the binding owner hands it over.
    let server = spawn_recording_mcp_server().await;
    let dir = tempdir().expect("temp dir");
    let config = tenant_owned_config(&server.url, dir.path());

    let session_tenant = tenant(0xab);
    let other_tenant = tenant(0xac);
    let reference = CredentialRef::from_uuid(Uuid::from_u128(0xcc));
    let foreign = binding(other_tenant, reference, Uuid::from_u128(0xdb));
    let router = router_with(
        &config,
        TenantMcpCredentialOwners {
            vault: Arc::new(StubVault::default().with_credential(
                reference,
                identity_of(&foreign),
                "other-tenant-secret",
            )),
            bindings: Arc::new(LyingBindings(foreign)),
            authorizer: Arc::new(StubAuthorizer::allowing()),
        },
    )
    .await;

    let error = router
        .execute_authorized(
            &session_for(session_tenant),
            &operator_of(session_tenant),
            &invocation(),
            ToolCallId::new(),
            None,
        )
        .await
        .expect_err("another tenant's binding must be refused");

    assert!(
        matches!(error, MoaError::PermissionDenied(ref message)
            if message.contains("does not belong to this tenant and server")),
        "expected the cross-tenant denial, got: {error}"
    );
    assert!(server.outbound_authorizations().is_empty());
}

#[tokio::test]
async fn a_tenant_owned_server_cannot_be_served_from_deployment_environment_offline() {
    // Pins: even with the deployment variable a deployment-owned twin would read
    // exported in this process, a tenant-owned server whose tenant path fails
    // returns that failure and never reaches the server with operator material.
    let token_env = format!("MOA_TEST_MCP_TOKEN_{}", Uuid::now_v7().simple());
    unsafe { std::env::set_var(&token_env, "deployment-secret") };

    let server = spawn_recording_mcp_server().await;
    let dir = tempdir().expect("temp dir");
    let config = tenant_owned_config(&server.url, dir.path());

    let tenant_id = tenant(0xad);
    let router = router_with(
        &config,
        TenantMcpCredentialOwners {
            vault: Arc::new(StubVault::default()),
            bindings: Arc::new(StubBindings::default()),
            authorizer: Arc::new(StubAuthorizer::allowing()),
        },
    )
    .await;

    let error = router
        .execute_authorized(
            &session_for(tenant_id),
            &operator_of(tenant_id),
            &invocation(),
            ToolCallId::new(),
            None,
        )
        .await
        .expect_err("a tenant-owned failure must not fall back");

    assert!(
        matches!(error, MoaError::PermissionDenied(ref message) if message.contains("no MCP connection binding")),
        "expected the tenant denial, got: {error}"
    );
    assert!(
        server.outbound_authorizations().is_empty(),
        "no deployment credential may be presented for a tenant-owned server"
    );
    unsafe { std::env::remove_var(token_env) };
}

#[tokio::test]
async fn a_tenant_owned_server_without_its_owners_fails_router_construction_offline() {
    // Pins: a deployment that configures a tenant-owned server without the vault,
    // binding owner, and authorizer fails at startup rather than at the first
    // tenant dispatch.
    let dir = tempdir().expect("temp dir");
    let config = tenant_owned_config("http://127.0.0.1:1", dir.path());

    let error = match ToolRouter::from_config(&config, Some(mcp_egress_guard()), None, None).await {
        Ok(_) => panic!("tenant-owned servers require their owners"),
        Err(error) => error,
    };

    assert!(
        error.to_string().contains(SERVER),
        "the error must name the offending server, got: {error}"
    );
    assert!(
        error.to_string().contains("tenant credential vault"),
        "the error must name the missing owners, got: {error}"
    );
}

#[tokio::test]
async fn a_tenant_owned_server_with_a_deployment_selector_fails_router_construction_offline() {
    // Pins: environment credentials remain available only to explicitly
    // deployment-owned connectors — configuring one on a tenant-owned server is
    // refused when the router is built.
    let dir = tempdir().expect("temp dir");
    let mut config = tenant_owned_config("http://127.0.0.1:1", dir.path());
    config.mcp_servers[0].credentials = Some(McpCredentialConfig::Bearer {
        token_env: "MOA_TEST_MCP_TOKEN_UNUSED".to_string(),
    });

    let error = match ToolRouter::from_config(
        &config,
        Some(mcp_egress_guard()),
        None,
        Some(TenantMcpCredentialOwners {
            vault: Arc::new(StubVault::default()),
            bindings: Arc::new(StubBindings::default()),
            authorizer: Arc::new(StubAuthorizer::allowing()),
        }),
    )
    .await
    {
        Ok(_) => panic!("a tenant-owned deployment selector must be refused"),
        Err(error) => error,
    };

    assert!(
        error
            .to_string()
            .contains("must not name a deployment environment variable"),
        "expected the ownership-mismatch error, got: {error}"
    );
}

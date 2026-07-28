//! Tenant MCP connection bindings against real Postgres, real row-level security,
//! and the real durable credential vault.
//!
//! The offline lane proves the router's decision order with in-memory doubles.
//! This lane proves the parts only a database can: that the forced-RLS policy —
//! not a `WHERE` clause — hides another tenant's binding, that the partial unique
//! index admits exactly one active binding per server, and that a dispatch
//! resolves a real encrypted credential version through the real vault, so
//! rotating to version N+1 changes what the very next call presents.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use moa_auth_providers::PostgresCredentialVault;
use moa_config::McpCredentialConfig;
use moa_config::McpServerConfig;
use moa_config::McpServerCredentialScope;
use moa_config::McpTransportConfig;
use moa_config::MoaConfig;
use moa_config::SecurityProfile;
use moa_core::error::{MoaError, Result};
use moa_core::traits::{CredentialVault, Identity, IdentityType};
use moa_core::types::credentials::{
    CredentialContext, CredentialIdentity, CredentialKind, CredentialOperation,
    CredentialPrincipal, CredentialRef,
};
use moa_core::types::identifiers::{ModelId, TenantId, ToolCallId};
use moa_core::types::memory::RlsContext;
use moa_core::types::security::SensitivityClass;
use moa_core::types::session::SessionMeta;
use moa_core::{types::completion::ToolInvocation, types::identifiers::SessionId};
use moa_crypto::{KeyManagementProvider, LocalKmsProvider};
use moa_db::ScopedConn;
use moa_hands::ToolRouter;
use moa_hands::core::mcp_connections::{
    PostgresTenantMcpConnectionBindings, TenantMcpAuthorizer, TenantMcpBindingStatus,
    TenantMcpConnectionBinding, TenantMcpConnectionBindingStore, TenantMcpCredentialOwners,
};
use moa_memory_pii::{MockClassifier, PiiResult};
use moa_security::McpEgressGuard;
use moa_test_support::fixtures::quote_identifier;
use secrecy::SecretString;
use serde_json::json;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

const SERVER: &str = "tenant-search";
const OPERATION: &str = "search_documents";

/// One isolated, migrated schema for a single test.
struct TestSchema {
    database_url: String,
    schema_name: String,
    pool: PgPool,
    /// One keyring for the whole fixture, as in production: the keys a vault
    /// wraps material with are shared durable state, so two independent local
    /// providers would model a split-brain keyring rather than a replica pair.
    kms: Arc<dyn KeyManagementProvider>,
}

impl TestSchema {
    /// Creates a uniquely named schema carrying the credential vault and binding
    /// migrations, so the vault and its bindings live in the same isolated place
    /// they occupy in the central schema.
    async fn new(prefix: &str) -> Self {
        let database_url = std::env::var("MOA_DATABASE_URL")
            .unwrap_or_else(|_| "postgres://moa_owner:dev@127.0.0.1:10040/moa".to_string());
        let schema_name = format!("{prefix}_{}", Uuid::new_v4().simple());
        let pool = connect_pool(&database_url, &schema_name).await;
        moa_migrations::run_auth_schema(&pool, &schema_name)
            .await
            .expect("auth baseline should apply");
        moa_migrations::run_hands_schema(&pool, &schema_name)
            .await
            .expect("tool-routing schema should apply");
        Self {
            database_url,
            schema_name,
            pool,
            kms: Arc::new(LocalKmsProvider::new()),
        }
    }

    fn bindings(&self) -> PostgresTenantMcpConnectionBindings {
        PostgresTenantMcpConnectionBindings::new(self.pool.clone())
    }

    fn vault(&self) -> PostgresCredentialVault {
        PostgresCredentialVault::new(Arc::new(self.pool.clone()), Arc::clone(&self.kms))
    }

    /// Drops the isolated schema. Called explicitly at the end of each test so a
    /// passing run leaves the shared development database as it found it.
    async fn cleanup(self) {
        let statement = format!(
            "DROP SCHEMA {} CASCADE",
            quote_identifier(&self.schema_name)
        );
        let _ = sqlx::query(&statement).execute(&self.pool).await;
        self.pool.close().await;
    }
}

impl Drop for TestSchema {
    /// Removes the schema when a test panicked before reaching `cleanup`.
    ///
    /// The whole cleanup is bounded and runs on its own runtime: this destructor
    /// blocks the test runtime's thread, so an unbounded await here would hang
    /// the failing test instead of reporting it. On elapse the schema is left to
    /// be swept manually rather than holding the run open.
    fn drop(&mut self) {
        let database_url = self.database_url.clone();
        let statement = format!(
            "DROP SCHEMA IF EXISTS {} CASCADE",
            quote_identifier(&self.schema_name)
        );
        let cleanup = std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async move {
                let _ = tokio::time::timeout(Duration::from_secs(10), async {
                    let pool = PgPoolOptions::new()
                        .max_connections(1)
                        .acquire_timeout(Duration::from_secs(5))
                        .connect(&database_url)
                        .await
                        .ok()?;
                    let _ = sqlx::query(&statement).execute(&pool).await;
                    pool.close().await;
                    Some(())
                })
                .await;
            });
        });
        let _ = cleanup.join();
    }
}

async fn connect_pool(database_url: &str, schema_name: &str) -> PgPool {
    let quoted = quote_identifier(schema_name);
    let create = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(database_url)
        .await
        .expect("test Postgres should be reachable");
    sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS {quoted}"))
        .execute(&create)
        .await
        .expect("create isolated schema");
    create.close().await;

    let search_path = format!("{quoted}, public");
    PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(move |conn, _meta| {
            let search_path = search_path.clone();
            Box::pin(async move {
                sqlx::query("SELECT pg_catalog.set_config('search_path', $1, false)")
                    .bind(search_path)
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(database_url)
        .await
        .expect("test Postgres should be reachable")
}

/// Authorizer that admits every caller.
///
/// Delegated tenant-operator authorization is pinned in the offline lane and by
/// the cross-tenant pentest suite; this lane isolates the storage and credential
/// behavior behind it.
struct AllowAuthorizer;

#[async_trait]
impl TenantMcpAuthorizer for AllowAuthorizer {
    async fn require_tenant_operator(
        &self,
        _identity: &Identity,
        _tenant_id: TenantId,
    ) -> Result<()> {
        Ok(())
    }
}

/// Fake MCP server that answers the handshake and records the `Authorization`
/// header of every `tools/call` it receives.
struct RecordingMcpServer {
    url: String,
    authorizations: Arc<Mutex<Vec<String>>>,
}

impl RecordingMcpServer {
    fn outbound_authorizations(&self) -> Vec<String> {
        self.authorizations
            .lock()
            .expect("read recorded authorizations")
            .clone()
    }
}

async fn spawn_recording_mcp_server() -> RecordingMcpServer {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake MCP server");
    let addr = listener.local_addr().expect("fake MCP server address");
    let authorizations = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&authorizations);
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
                    recorded
                        .lock()
                        .expect("record outbound authorization")
                        .push(authorization);
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
        authorizations,
    }
}

fn binding(
    tenant_id: TenantId,
    connection_uid: Uuid,
    credential_ref: CredentialRef,
) -> TenantMcpConnectionBinding {
    TenantMcpConnectionBinding {
        tenant_id,
        connection_uid,
        server_name: SERVER.to_string(),
        credential_ref,
        status: TenantMcpBindingStatus::Active,
        allowed_operations: vec![OPERATION.to_string()],
    }
}

fn credential_context(
    tenant_id: TenantId,
    connection_uid: Uuid,
    operation: CredentialOperation,
    label: &str,
) -> CredentialContext {
    CredentialContext {
        tenant_id,
        principal: CredentialPrincipal::Caller {
            identity_id: Uuid::new_v4(),
            delegated_by: None,
        },
        operation,
        operation_id: format!("{label}:{connection_uid}"),
        request_hash: format!("hash:{label}:{connection_uid}"),
    }
}

/// Stores one MCP credential version for a tenant connection and returns its
/// opaque reference.
async fn store_credential(
    vault: &PostgresCredentialVault,
    tenant_id: TenantId,
    connection_uid: Uuid,
    material: &str,
    label: &str,
) -> CredentialRef {
    vault
        .create(
            CredentialIdentity {
                tenant_id,
                connection_uid,
                kind: CredentialKind::McpBearer,
            },
            SecretString::from(material.to_string()),
            &credential_context(
                tenant_id,
                connection_uid,
                CredentialOperation::Create,
                label,
            ),
        )
        .await
        .expect("credential version should be created")
        .reference
}

fn session_for(tenant_id: TenantId) -> SessionMeta {
    SessionMeta {
        id: SessionId::new(),
        tenant_id,
        model: ModelId::new("claude-sonnet-4-6"),
        ..SessionMeta::default()
    }
}

fn operator_of(tenant_id: TenantId) -> Identity {
    Identity {
        identity_type: IdentityType::Operator,
        id: Uuid::new_v4(),
        tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

fn invocation() -> ToolInvocation {
    ToolInvocation {
        id: None,
        name: OPERATION.to_string(),
        input: json!({}),
    }
}

fn egress_guard() -> Arc<McpEgressGuard> {
    Arc::new(McpEgressGuard::new(Arc::new(MockClassifier {
        fixed: PiiResult {
            class: SensitivityClass::None,
            spans: Vec::new(),
            model_version: "tenant-mcp-db-test".to_string(),
            abstained: false,
        },
    })))
}

/// Builds the real router over the real binding owner and credential vault.
async fn router_for(schema: &TestSchema, sandbox_dir: &std::path::Path, url: &str) -> ToolRouter {
    let mut config = MoaConfig::default();
    config.local.docker_enabled = false;
    config.security_profile = SecurityProfile::Local;
    config.local.sandbox_dir = sandbox_dir.join("sandbox").display().to_string();
    config.mcp_servers = vec![McpServerConfig {
        name: SERVER.to_string(),
        transport: McpTransportConfig::Http,
        url: Some(url.to_string()),
        credential_scope: McpServerCredentialScope::TenantOwned,
        credentials: Some(McpCredentialConfig::TenantBearer),
        trust_tool_annotations: false,
        allowed_data_classes: Vec::new(),
    }];

    ToolRouter::from_config(
        &config,
        Some(egress_guard()),
        None,
        Some(TenantMcpCredentialOwners {
            vault: Arc::new(schema.vault()),
            bindings: Arc::new(schema.bindings()),
            authorizer: Arc::new(AllowAuthorizer),
        }),
    )
    .await
    .expect("tenant-owned router should build")
}

#[tokio::test]
async fn another_tenants_binding_is_invisible_under_forced_row_level_security_db() {
    // Pins: tenant isolation comes from the table's forced row-level-security
    // policy, not from a query predicate. Reading with the other tenant's scope
    // installed — and with no tenant predicate at all — returns nothing.
    let schema = TestSchema::new("mcp_binding_rls").await;
    let bindings = schema.bindings();
    let owner = TenantId::new();
    let intruder = TenantId::new();
    bindings
        .upsert_binding(&binding(
            owner,
            Uuid::new_v4(),
            CredentialRef::from_uuid(Uuid::new_v4()),
        ))
        .await
        .expect("seed the owning tenant's binding");

    let visible_to_owner = bindings
        .binding_for_server(owner, SERVER)
        .await
        .expect("owner read succeeds");
    let visible_to_intruder = bindings
        .binding_for_server(intruder, SERVER)
        .await
        .expect("intruder read succeeds");

    let mut conn = ScopedConn::begin_as_app(&schema.pool, &RlsContext::tenant(intruder), true)
        .await
        .expect("scoped connection");
    let unscoped_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tenant_mcp_connection_bindings")
            .fetch_one(conn.as_mut())
            .await
            .expect("count without a tenant predicate");
    conn.commit().await.expect("commit read");

    assert!(visible_to_owner.is_some(), "the owner must see its binding");
    assert!(
        visible_to_intruder.is_none(),
        "another tenant must not see the binding"
    );
    assert_eq!(
        unscoped_rows, 0,
        "the policy alone must hide the row from an unrelated tenant"
    );
    schema.cleanup().await;
}

#[tokio::test]
async fn only_one_active_binding_per_server_is_accepted_db() {
    // Pins: the partial unique active index makes "which connection serves this
    // server" unambiguous — a second active binding is rejected by the database,
    // while a disabled binding for the same server may be retained as history.
    let schema = TestSchema::new("mcp_binding_unique").await;
    let bindings = schema.bindings();
    let tenant_id = TenantId::new();
    let first_connection = Uuid::new_v4();
    let second_connection = Uuid::new_v4();
    bindings
        .upsert_binding(&binding(
            tenant_id,
            first_connection,
            CredentialRef::from_uuid(Uuid::new_v4()),
        ))
        .await
        .expect("first active binding");

    let conflict = bindings
        .upsert_binding(&binding(
            tenant_id,
            second_connection,
            CredentialRef::from_uuid(Uuid::new_v4()),
        ))
        .await
        .expect_err("a second active binding for one server must be rejected");

    let mut disabled = binding(
        tenant_id,
        second_connection,
        CredentialRef::from_uuid(Uuid::new_v4()),
    );
    disabled.status = TenantMcpBindingStatus::Disabled;
    bindings
        .upsert_binding(&disabled)
        .await
        .expect("a disabled binding for the same server is retained history");

    assert!(
        matches!(conflict, MoaError::StorageError(ref message)
            if message.contains("tenant_mcp_connection_bindings_one_active_server")),
        "expected the partial unique index violation, got: {conflict}"
    );
    let served = bindings
        .binding_for_server(tenant_id, SERVER)
        .await
        .expect("read the served binding")
        .expect("an active binding exists");
    assert_eq!(
        served.connection_uid, first_connection,
        "the active binding, not the disabled sibling, serves the server"
    );
    schema.cleanup().await;
}

#[tokio::test]
async fn a_disabled_binding_is_reported_as_disabled_rather_than_missing_db() {
    // Pins: disabling a connection is distinguishable from never having one, so
    // an operator surface can tell "turned off" from "unknown" while dispatch
    // denies both.
    let schema = TestSchema::new("mcp_binding_disabled").await;
    let bindings = schema.bindings();
    let tenant_id = TenantId::new();
    let mut disabled = binding(
        tenant_id,
        Uuid::new_v4(),
        CredentialRef::from_uuid(Uuid::new_v4()),
    );
    disabled.status = TenantMcpBindingStatus::Disabled;
    bindings
        .upsert_binding(&disabled)
        .await
        .expect("seed a disabled binding");

    let loaded = bindings
        .binding_for_server(tenant_id, SERVER)
        .await
        .expect("read the disabled binding")
        .expect("a disabled binding is still returned");

    assert_eq!(loaded.status, TenantMcpBindingStatus::Disabled);
    assert_eq!(loaded.allowed_operations, vec![OPERATION.to_string()]);
    schema.cleanup().await;
}

#[tokio::test]
async fn tenant_purge_removes_only_that_tenants_bindings_db() {
    // Pins: the bounded tenant sweep the purge lifecycle calls removes the
    // tenant's bindings and terminates, and never touches another tenant's rows.
    let schema = TestSchema::new("mcp_binding_purge").await;
    let bindings = schema.bindings();
    let purged = TenantId::new();
    let retained = TenantId::new();
    for server in ["tenant-search", "tenant-filings", "tenant-mail"] {
        let mut row = binding(
            purged,
            Uuid::new_v4(),
            CredentialRef::from_uuid(Uuid::new_v4()),
        );
        row.server_name = server.to_string();
        bindings.upsert_binding(&row).await.expect("seed binding");
    }
    bindings
        .upsert_binding(&binding(
            retained,
            Uuid::new_v4(),
            CredentialRef::from_uuid(Uuid::new_v4()),
        ))
        .await
        .expect("seed the retained tenant's binding");

    let mut removed_total = 0_u64;
    loop {
        let removed = bindings
            .purge_tenant_bindings(purged, 2)
            .await
            .expect("bounded purge batch");
        if removed == 0 {
            break;
        }
        removed_total += removed;
    }

    assert_eq!(
        removed_total, 3,
        "every binding of the purged tenant is gone"
    );
    assert!(
        bindings
            .binding_for_server(purged, SERVER)
            .await
            .expect("read after purge")
            .is_none()
    );
    assert!(
        bindings
            .binding_for_server(retained, SERVER)
            .await
            .expect("read the retained tenant")
            .is_some(),
        "another tenant's binding must survive the purge"
    );
    schema.cleanup().await;
}

#[tokio::test]
async fn two_tenants_sharing_one_server_present_only_their_own_stored_credential_db() {
    // Pins: the end-to-end least-privilege guarantee against real storage — two
    // tenants share one configured MCP server, each binding names its own
    // encrypted credential version, and each dispatch presents only that tenant's
    // decrypted material.
    let schema = TestSchema::new("mcp_two_tenants").await;
    let vault = schema.vault();
    let bindings = schema.bindings();
    let server = spawn_recording_mcp_server().await;
    let dir = tempdir().expect("temp dir");

    let first = TenantId::new();
    let second = TenantId::new();
    let first_connection = Uuid::new_v4();
    let second_connection = Uuid::new_v4();
    let first_reference =
        store_credential(&vault, first, first_connection, "first-secret", "first").await;
    let second_reference =
        store_credential(&vault, second, second_connection, "second-secret", "second").await;
    bindings
        .upsert_binding(&binding(first, first_connection, first_reference))
        .await
        .expect("bind the first tenant");
    bindings
        .upsert_binding(&binding(second, second_connection, second_reference))
        .await
        .expect("bind the second tenant");

    let router = router_for(&schema, dir.path(), &server.url).await;
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
            .expect("a bound tenant dispatches");
        let output = secured.safe_output;
        assert_eq!(output.to_text(), "pong");
    }

    assert_eq!(
        server.outbound_authorizations(),
        vec![
            "Bearer first-secret".to_string(),
            "Bearer second-secret".to_string()
        ],
        "each tenant must present only its own stored credential"
    );
    schema.cleanup().await;
}

#[tokio::test]
async fn rotating_the_binding_to_the_next_version_affects_the_next_call_without_restart_db() {
    // Pins: rotation is durable state, not process state. Rotating the vault
    // version and repointing the binding changes what the very next dispatch
    // presents on the same live router, and the superseded version is refused if
    // a binding is left pointing at it.
    let schema = TestSchema::new("mcp_rotation").await;
    let vault = schema.vault();
    let bindings = schema.bindings();
    let server = spawn_recording_mcp_server().await;
    let dir = tempdir().expect("temp dir");

    let tenant_id = TenantId::new();
    let connection_uid = Uuid::new_v4();
    let first_reference =
        store_credential(&vault, tenant_id, connection_uid, "version-one", "rotate").await;
    bindings
        .upsert_binding(&binding(tenant_id, connection_uid, first_reference))
        .await
        .expect("bind the first version");

    let router = router_for(&schema, dir.path(), &server.url).await;
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

    let next_reference = vault
        .rotate(
            first_reference,
            SecretString::from("version-two".to_string()),
            &credential_context(
                tenant_id,
                connection_uid,
                CredentialOperation::Rotate,
                "rotate-next",
            ),
        )
        .await
        .expect("rotate to the next version")
        .reference;

    // A binding still pointing at the superseded version is refused before any
    // outbound call, rather than presenting an outdated secret.
    let stale_error = router
        .execute_authorized(
            &session_for(tenant_id),
            &operator_of(tenant_id),
            &invocation(),
            ToolCallId::new(),
            None,
        )
        .await
        .expect_err("a superseded version must be refused");
    assert!(
        matches!(stale_error, MoaError::PermissionDenied(ref message) if message.contains("stale")),
        "expected the stale-version denial, got: {stale_error}"
    );

    bindings
        .upsert_binding(&binding(tenant_id, connection_uid, next_reference))
        .await
        .expect("repoint the binding at the next version");
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
        "the repointed binding takes effect on the next call without a restart"
    );
    schema.cleanup().await;
}

#[tokio::test]
async fn a_binding_naming_another_connections_credential_is_refused_before_resolution_db() {
    // Pins: reference drift is caught against the stored version's own identity.
    // A binding that names a credential belonging to a different connection of
    // the same tenant is refused, and the material is never opened.
    let schema = TestSchema::new("mcp_reference_drift").await;
    let vault = schema.vault();
    let bindings = schema.bindings();
    let server = spawn_recording_mcp_server().await;
    let dir = tempdir().expect("temp dir");

    let tenant_id = TenantId::new();
    let bound_connection = Uuid::new_v4();
    let other_connection = Uuid::new_v4();
    let other_reference = store_credential(
        &vault,
        tenant_id,
        other_connection,
        "other-connection-secret",
        "drift",
    )
    .await;
    bindings
        .upsert_binding(&binding(tenant_id, bound_connection, other_reference))
        .await
        .expect("bind a reference from another connection");

    let router = router_for(&schema, dir.path(), &server.url).await;
    let error = router
        .execute_authorized(
            &session_for(tenant_id),
            &operator_of(tenant_id),
            &invocation(),
            ToolCallId::new(),
            None,
        )
        .await
        .expect_err("a drifted reference must be refused");

    assert!(
        matches!(error, MoaError::PermissionDenied(ref message) if message.contains("expected connection")),
        "expected the wrong-connection denial, got: {error}"
    );
    assert!(
        server.outbound_authorizations().is_empty(),
        "a drifted reference must never reach the server"
    );
    schema.cleanup().await;
}

#[tokio::test]
async fn a_cross_tenant_binding_cannot_serve_another_tenants_credential_db() {
    // Pins: one tenant's session cannot reach another tenant's stored credential
    // even when both are bound to the same server name in the same database.
    let schema = TestSchema::new("mcp_cross_tenant").await;
    let vault = schema.vault();
    let bindings = schema.bindings();
    let server = spawn_recording_mcp_server().await;
    let dir = tempdir().expect("temp dir");

    let bound = TenantId::new();
    let unbound = TenantId::new();
    let connection_uid = Uuid::new_v4();
    let reference = store_credential(&vault, bound, connection_uid, "bound-secret", "cross").await;
    bindings
        .upsert_binding(&binding(bound, connection_uid, reference))
        .await
        .expect("bind the owning tenant");

    let router = router_for(&schema, dir.path(), &server.url).await;
    let error = router
        .execute_authorized(
            &session_for(unbound),
            &operator_of(unbound),
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
        "a tenant without a binding must never reach the server"
    );
    schema.cleanup().await;
}

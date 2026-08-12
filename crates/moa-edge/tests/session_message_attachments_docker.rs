//! Docker-gated multipart attachment behavior for the contact session-message route.
//!
//! Requires Postgres and the RustFS object store from docker compose, because the whole
//! point is what happens to durable attachment rows and stored objects when a submission
//! is retried or rejected.

use std::sync::Arc;

use axum::Router;
use axum::response::IntoResponse;
use moa_config::MoaConfig;
use moa_config::SessionAttachmentStorageConfig;
use moa_core::{
    traits::AuthError, traits::AuthProvider, traits::Credential, traits::Identity,
    traits::IdentityType, traits::SessionAttachmentStore, traits::SessionStore,
    types::agent::AgentContext, types::contact::SessionActorRef, types::identifiers::ModelId,
    types::identifiers::SessionId, types::identifiers::TenantId, types::session::SessionMeta,
};
use moa_edge::proxy::OrchestratorProxy;
use moa_edge::routes::{self, AppState, KnowledgeWebhookEdgeConfig};
use moa_session::{PostgresSessionStore, testing};
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Accepts common truthy values so a developer's `.env` enables this lane.
fn require_docker_tests_enabled() {
    let enabled = std::env::var("MOA_RUN_SESSION_ATTACHMENT_DOCKER_TESTS")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false);
    if !enabled {
        panic!(
            "set MOA_RUN_SESSION_ATTACHMENT_DOCKER_TESTS=1 to run RustFS session attachment tests"
        );
    }
}

#[derive(Clone)]
struct FixedAuth {
    identity: Identity,
}

#[async_trait::async_trait]
impl AuthProvider for FixedAuth {
    async fn authenticate(&self, _credential: &Credential) -> Result<Identity, AuthError> {
        Ok(self.identity.clone())
    }

    fn name(&self) -> &'static str {
        "fixed-test"
    }

    fn requires_credentials(&self) -> bool {
        false
    }
}

struct ContactsUpstream {
    base_url: String,
    submissions: Arc<Mutex<Vec<Value>>>,
    server: JoinHandle<()>,
}

/// Starts an upstream that admits every message except `conflicting_id`.
async fn start_contacts_upstream(conflicting_id: Option<&str>) -> ContactsUpstream {
    let submissions = Arc::new(Mutex::new(Vec::new()));
    let recorded = submissions.clone();
    let conflicting_id = conflicting_id.map(ToOwned::to_owned);
    let app = Router::new().fallback(move |request: axum::extract::Request| {
        let recorded = recorded.clone();
        let conflicting_id = conflicting_id.clone();
        async move {
            let path = request.uri().path().to_string();
            let body = axum::body::to_bytes(request.into_body(), 32 * 1024 * 1024)
                .await
                .expect("read upstream body");
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            if path.ends_with("/Contacts/progress") {
                return axum::Json(json!({
                    "snapshot": {
                        "session_id": body["session_id"],
                        "active_turn_id": null,
                        "pending_message_count": 0,
                        "last_outcome": null,
                        "active_execution_run_uids": [],
                    },
                    "events": [],
                }))
                .into_response();
            }
            recorded.lock().await.push(body.clone());
            let client_message_id = body["client_message_id"].as_str().unwrap_or_default();
            if conflicting_id.as_deref() == Some(client_message_id) {
                return (
                    axum::http::StatusCode::CONFLICT,
                    "client message id was already admitted for a different request",
                )
                    .into_response();
            }
            axum::Json(json!({
                "session_id": body["session_id"],
                "queued": false,
                "started_turn_id": "turn-1",
                "stream_cursor": body["stream_cursor"],
            }))
            .into_response()
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind contacts upstream");
    let addr = listener.local_addr().expect("read upstream addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve upstream");
    });
    ContactsUpstream {
        base_url: format!("http://{addr}"),
        submissions,
        server,
    }
}

/// Local RustFS attachment storage for this lane, honoring the compose host port.
///
/// `docker-compose` publishes RustFS on `MOA_RUSTFS_PORT` because 9000 is a popular port
/// (minio, k3d node ports), so a developer whose host already uses 9000 maps it elsewhere.
/// The lane resolves the same `MOA_OBJECT_STORE_ENDPOINT` the deployment overlay
/// uses, then that published port, before falling back to the documented default.
fn configure_local_attachment_storage(config: &mut MoaConfig) {
    config.session.attachments = SessionAttachmentStorageConfig::default();
    config.object_store = moa_config::ObjectStoreConfig::local_rustfs();
    if let Some(endpoint) = std::env::var("MOA_OBJECT_STORE_ENDPOINT")
        .ok()
        .filter(|endpoint| !endpoint.trim().is_empty())
    {
        config.object_store.endpoint = Some(endpoint);
    } else if let Some(port) = std::env::var("MOA_RUSTFS_PORT")
        .ok()
        .filter(|port| !port.trim().is_empty())
    {
        config.object_store.endpoint = Some(format!("http://127.0.0.1:{}", port.trim()));
    }
}

/// Provisions an isolated Postgres schema whose store writes to local RustFS.
async fn create_attachment_test_store() -> (PostgresSessionStore, String, String) {
    let (database_url, schema_name) = testing::provision_cloned_database()
        .await
        .expect("provision isolated attachment test database");
    let pool = PgPoolOptions::new()
        .min_connections(1)
        .max_connections(4)
        .connect(&database_url)
        .await
        .expect("connect attachment test pool");
    let mut config = MoaConfig::default();
    config.database.url = database_url.clone();
    config.database.schema = Some(schema_name.clone());
    configure_local_attachment_storage(&mut config);
    let store = PostgresSessionStore::from_existing_pool_with_config(&config, pool)
        .await
        .expect("create attachment test store");
    (store, database_url, schema_name)
}

struct EdgeServer {
    base_url: String,
    server: JoinHandle<()>,
}

async fn start_edge(
    store: &PostgresSessionStore,
    database_url: &str,
    schema_name: &str,
    tenant_id: TenantId,
    upstream: &str,
) -> EdgeServer {
    let mut config = MoaConfig::default();
    config.database.url = database_url.to_string();
    config.database.schema = Some(schema_name.to_string());
    let pool = Arc::new(store.pool().clone());
    let oauth_server = Arc::new(
        moa_auth_providers::OAuthServer::from_config(&config.auth.oauth, pool.clone())
            .await
            .expect("bootstrap edge OAuth server"),
    );
    let audit = moa_ocsf::AuditRuntime::start(pool.as_ref().clone())
        .expect("edge test audit runtime should start");
    let state = AppState {
        connector_management_enabled: false,
        // The audit writer is owned by this test for its lifetime; dropping the
        // runtime aborts it, which is the same ownership the binary has.
        audit: audit.emitter(),
        config: Arc::new(config),
        auth: Arc::new(FixedAuth {
            identity: Identity {
                identity_type: IdentityType::Contact,
                id: Uuid::now_v7(),
                tenant_id,
                api_key_id: None,
                acting_on_behalf_of: None,
            },
        }),
        oauth_server,
        oauth_access_tokens: Arc::new(moa_auth_providers::OAuthAccessTokenProvider::new(
            pool.clone(),
        )),
        fga: None,
        knowledge_webhooks: KnowledgeWebhookEdgeConfig::default(),
        pool,
        session_store: Arc::new(store.clone()),
        delivery: Arc::new(moa_messaging::ProviderDeliverySink::empty(
            "edge-tests@example.invalid",
        )),
        proxy: Arc::new(OrchestratorProxy::new(upstream).expect("proxy URL is valid")),
        connector_credentials: Arc::new(
            moa_edge::connector_credential_proxy::ConnectorCredentialProxy::new(upstream)
                .expect("credential proxy URL is valid"),
        ),
        external_job_callbacks: Arc::new(
            moa_edge::external_job_callback_proxy::ExternalJobCallbackProxy::new(upstream)
                .expect("callback proxy URL is valid"),
        ),
        clickhouse_lineage: None,
        clickhouse_analytics: None,
    };
    let app = routes::router(state);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind edge test server");
    let addr = listener.local_addr().expect("read edge addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve edge router");
    });
    EdgeServer {
        base_url: format!("http://{addr}"),
        server,
    }
}

/// Posts one multipart session message carrying a single photo.
async fn post_photo_message(
    client: &reqwest::Client,
    edge: &EdgeServer,
    tenant_id: TenantId,
    session_id: SessionId,
    client_message_id: &str,
    text: &str,
    photo: Vec<u8>,
) -> reqwest::Response {
    let form = reqwest::multipart::Form::new()
        .text("tenant_id", tenant_id.0.to_string())
        .text("contact_token", "contact-token")
        .text("client_message_id", client_message_id.to_string())
        .text("user_message", text.to_string())
        .part(
            "photo",
            reqwest::multipart::Part::bytes(photo)
                .file_name("receipt.png")
                .mime_str("image/png")
                .expect("photo part mime is valid"),
        );
    client
        .post(format!(
            "{}/v1/sessions/{session_id}/messages",
            edge.base_url
        ))
        .header("accept", "text/event-stream")
        .multipart(form)
        .send()
        .await
        .expect("send multipart session message")
}

fn png_with_dimensions(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Vec::from(&b"\x89PNG\r\n\x1a\n"[..]);
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    append_png_chunk(&mut bytes, b"IHDR", &ihdr);
    append_png_chunk(
        &mut bytes,
        b"IDAT",
        &[0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01],
    );
    append_png_chunk(&mut bytes, b"IEND", &[]);
    bytes
}

fn append_png_chunk(bytes: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
    bytes.extend_from_slice(kind);
    bytes.extend_from_slice(data);
    bytes.extend_from_slice(&[0, 0, 0, 0]);
}

async fn create_session(store: &PostgresSessionStore, tenant_id: TenantId) -> SessionId {
    store
        .create_session(SessionMeta {
            tenant_id,
            created_by: Some(SessionActorRef::Identity { id: Uuid::now_v7() }),
            model: ModelId::new("test-model"),
            agent_context: Some(AgentContext::system_default()),
            ..SessionMeta::default()
        })
        .await
        .expect("create session for attachment upload")
}

#[tokio::test]
#[ignore = "requires RustFS/object storage"]
async fn retried_photo_upload_reuses_one_attachment_and_stored_object_docker() {
    // Pins: a disconnect before the first response is the exact case this task exists for.
    // Re-posting the same client message id with the same photo must leave one attachment
    // row and one stored object, and must forward the identical attachment metadata, rather
    // than charging the user for a second upload of the same picture.
    require_docker_tests_enabled();
    let (store, database_url, schema_name) = create_attachment_test_store().await;
    let tenant_id = TenantId::from(Uuid::now_v7());
    let session_id = create_session(&store, tenant_id).await;
    let upstream = start_contacts_upstream(None).await;
    let edge = start_edge(
        &store,
        &database_url,
        &schema_name,
        tenant_id,
        &upstream.base_url,
    )
    .await;
    let client = reqwest::Client::new();
    let photo = png_with_dimensions(64, 48);

    for attempt in 0..2 {
        let response = post_photo_message(
            &client,
            &edge,
            tenant_id,
            session_id,
            "client-message-photo",
            "here is the receipt",
            photo.clone(),
        )
        .await;
        assert_eq!(
            response.status(),
            reqwest::StatusCode::OK,
            "attempt {attempt} should be admitted"
        );
        drop(response);
    }

    let stored = store
        .list_for_session(tenant_id, session_id)
        .await
        .expect("list session attachments");
    assert_eq!(
        stored.len(),
        1,
        "a retried upload must not create a second attachment"
    );
    let submissions = upstream.submissions.lock().await;
    assert_eq!(submissions.len(), 2);
    assert_eq!(
        submissions[0]["attachments"], submissions[1]["attachments"],
        "the retry must forward the replayed attachment, not a new one"
    );
    drop(submissions);

    let attachment_id = stored[0].id.expect("stored attachment has an id");
    let (_, content) = store
        .get(tenant_id, session_id, attachment_id)
        .await
        .expect("read stored attachment content");
    assert_eq!(content, photo, "the stored object must be the photo bytes");

    stop(edge.server, upstream.server).await;
    store.pool().close().await;
    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop test schema");
}

#[tokio::test]
#[ignore = "requires RustFS/object storage"]
async fn rejected_message_cleans_up_only_the_attachments_it_created_docker() {
    // Pins: rejection cleanup is scoped to this request's own writes. A first submission's
    // attachment must survive a later rejected retry that merely replayed it — deleting the
    // replayed original would leave the live message pointing at nothing — while a rejected
    // first submission must leave nothing behind.
    require_docker_tests_enabled();
    let (store, database_url, schema_name) = create_attachment_test_store().await;
    let tenant_id = TenantId::from(Uuid::now_v7());
    let session_id = create_session(&store, tenant_id).await;
    let upstream = start_contacts_upstream(Some("client-message-rejected")).await;
    let edge = start_edge(
        &store,
        &database_url,
        &schema_name,
        tenant_id,
        &upstream.base_url,
    )
    .await;
    let client = reqwest::Client::new();
    let photo = png_with_dimensions(64, 48);

    let rejected = post_photo_message(
        &client,
        &edge,
        tenant_id,
        session_id,
        "client-message-rejected",
        "rejected upload",
        photo.clone(),
    )
    .await;
    assert_eq!(rejected.status(), reqwest::StatusCode::CONFLICT);
    assert!(
        store
            .list_for_session(tenant_id, session_id)
            .await
            .expect("list after rejection")
            .is_empty(),
        "a rejected first submission must not leave an attachment behind"
    );

    let accepted = post_photo_message(
        &client,
        &edge,
        tenant_id,
        session_id,
        "client-message-accepted",
        "accepted upload",
        photo.clone(),
    )
    .await;
    assert_eq!(accepted.status(), reqwest::StatusCode::OK);
    drop(accepted);
    let original = store
        .list_for_session(tenant_id, session_id)
        .await
        .expect("list after acceptance");
    assert_eq!(original.len(), 1);

    // Same message identity and same photo, but the fence now rejects the submission. The
    // attachment write replays the original, so cleanup must leave it alone.
    let upstream_conflict = start_contacts_upstream(Some("client-message-accepted")).await;
    let conflicted_edge = start_edge(
        &store,
        &database_url,
        &schema_name,
        tenant_id,
        &upstream_conflict.base_url,
    )
    .await;
    let conflicted = post_photo_message(
        &client,
        &conflicted_edge,
        tenant_id,
        session_id,
        "client-message-accepted",
        "accepted upload",
        photo.clone(),
    )
    .await;
    assert_eq!(conflicted.status(), reqwest::StatusCode::CONFLICT);

    let after_conflict = store
        .list_for_session(tenant_id, session_id)
        .await
        .expect("list after replayed rejection");
    assert_eq!(
        after_conflict, original,
        "rejection cleanup must never delete a replayed original attachment"
    );
    let attachment_id = original[0].id.expect("stored attachment has an id");
    let (_, content) = store
        .get(tenant_id, session_id, attachment_id)
        .await
        .expect("original attachment content survives the rejected retry");
    assert_eq!(content, photo);

    stop(conflicted_edge.server, upstream_conflict.server).await;
    stop(edge.server, upstream.server).await;
    store.pool().close().await;
    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop test schema");
}

async fn stop(edge: JoinHandle<()>, upstream: JoinHandle<()>) {
    edge.abort();
    upstream.abort();
    let _ = edge.await;
    let _ = upstream.await;
}

//! Offline coverage for the tenant Knowledge service application surface.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use chrono::Utc;
use moa_core::{
    TenantId,
    wire::knowledge::{
        KnowledgeExchangeTokenRequest, KnowledgeObjectInspectRequest, KnowledgeObjectListRequest,
        KnowledgeProviderWebhookRequest, KnowledgeQueryTraceRequest, KnowledgeSyncRequest,
    },
};
use moa_knowledge::{
    Error as KnowledgeError,
    domain::{
        ConnectionStatus, ContactGroup, ContactGroupMembership, CreateLinkTokenRequest,
        DocumentVersion, ExchangePublicTokenRequest, KnowledgeBlock, KnowledgeChunk,
        KnowledgeConnection, KnowledgeConnectionProjection, KnowledgeIngestionStep,
        KnowledgeObject, KnowledgeObjectInspection, KnowledgeObjectProjection,
        KnowledgeProviderEventRecord, KnowledgeSyncCounters, KnowledgeSyncRun, LinkToken,
        LinkedAccount, ListChangedRecordsRequest, ObjectStatus, RecordPage, TriggerSyncRequest,
        TriggeredSync, WebhookEvent,
    },
    providers::LinkedIntegrationProvider,
    repository::KnowledgeRepository,
};
use moa_orchestrator::services::knowledge::{
    KnowledgeCredentialStore, KnowledgeService, StaticKnowledgeProviders,
};
use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use tokio_util::bytes::Bytes;
use uuid::Uuid;

const PROVIDER: &str = "fake";
const CONNECTOR: &str = "drive";
const SECRET_TOKEN: &str = "provider-secret-token-123";
const SECRET_BEARER: &str = "Bearer provider-secret-token-456";
const RAW_DOCUMENT_TAIL: &str = "RAW_FULL_DOCUMENT_TAIL_SHOULD_NOT_APPEAR";
const OTHER_CONTACT_MEMORY: &str = "other contact private memory should not appear";

#[tokio::test]
async fn manual_sync_triggers_provider_and_does_not_ingest_inline() {
    // Pins: manual sync returns after provider trigger and only touches sync-run state.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection = fixture_connection(tenant_id);
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    repository
        .insert_connection(connection.clone())
        .expect("fixture connection should be inserted");
    let provider = Arc::new(FakeLinkedIntegrationProvider::default());
    let service = fixture_service(repository.clone(), provider.clone(), 80);

    let response = service
        .sync_connection(KnowledgeSyncRequest {
            tenant_id,
            connection_uid: connection.connection_uid,
            parser: Some("native".to_string()),
            max_records: Some(25),
        })
        .await
        .expect("manual sync should trigger provider sync");

    assert_eq!(response.status, "running");
    assert_eq!(provider.trigger_sync_count(), 1);
    assert_eq!(provider.list_changed_records_count(), 0);
    assert_eq!(repository.op_count("create_sync_run"), 1);
    assert_eq!(repository.op_count("update_sync_run"), 1);
    assert_eq!(repository.op_count("record_ingestion_step"), 1);
    assert_eq!(repository.op_count("upsert_object"), 0);
    assert_eq!(repository.op_count("insert_document_version"), 0);
    assert_eq!(repository.op_count("replace_blocks"), 0);
    assert_eq!(repository.op_count("replace_chunks"), 0);
    assert_eq!(repository.op_count("set_chunk_graph_uid"), 0);
    assert_eq!(repository.op_count("add_sync_counters"), 0);
    assert_eq!(repository.sync_run_count(), 1);
    assert_eq!(repository.step_count(), 1);
}

#[tokio::test]
async fn duplicate_provider_webhook_records_once_and_enqueues_once() {
    // Pins: duplicate provider deliveries are idempotent and enqueue ingestion only once.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection = fixture_connection(tenant_id);
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    repository
        .insert_connection(connection.clone())
        .expect("fixture connection should be inserted");
    let provider = Arc::new(FakeLinkedIntegrationProvider::default());
    let service = fixture_service(repository.clone(), provider.clone(), 80);
    let request = webhook_request(tenant_id, connection.connection_uid, "evt-duplicate");

    let first = service
        .provider_webhook(request.clone())
        .await
        .expect("first webhook delivery should be accepted");
    let second = service
        .provider_webhook(request)
        .await
        .expect("duplicate webhook delivery should be accepted idempotently");

    assert!(!first.duplicate);
    assert!(first.ingestion_enqueued);
    assert!(first.sync_run_uid.is_some());
    assert!(second.duplicate);
    assert!(!second.ingestion_enqueued);
    assert!(second.sync_run_uid.is_none());
    assert_eq!(provider.verify_webhook_count(), 2);
    assert_eq!(repository.provider_event_count(), 1);
    assert_eq!(repository.sync_run_count(), 1);
    assert_eq!(repository.step_count(), 1);
}

#[tokio::test]
async fn exchange_stores_only_credential_reference_on_connection() {
    // Pins: public-token exchange persists credential material through the credential store only.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    let provider = Arc::new(FakeLinkedIntegrationProvider::default());
    let credentials = Arc::new(FakeKnowledgeCredentialStore::default());
    let service = KnowledgeService::new(
        repository.clone(),
        Arc::new(StaticKnowledgeProviders::new().with_provider(PROVIDER, provider.clone())),
        credentials.clone(),
        80,
    );

    let response = service
        .exchange_public_token(KnowledgeExchangeTokenRequest {
            tenant_id,
            provider: PROVIDER.to_string(),
            exchange_token: "public-token".to_string(),
        })
        .await
        .expect("token exchange should persist a connection");
    let connection = repository
        .connection(response.connection_uid)
        .expect("connection should be stored");

    assert_eq!(provider.exchange_count(), 1);
    assert_eq!(credentials.stored_account_count(), 1);
    assert_eq!(
        connection.credential_ref,
        credentials.vault_ref_for(tenant_id)
    );
    assert_ne!(connection.credential_ref, SECRET_TOKEN);
    assert!(!connection.credential_ref.contains(SECRET_TOKEN));
    assert_eq!(response.provider, PROVIDER);
    assert_eq!(response.connector, CONNECTOR);
}

#[tokio::test]
async fn list_and_inspect_redact_tokens_and_bound_previews() {
    // Pins: inspection/listing APIs expose safe metadata and bounded text previews only.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection = fixture_connection(tenant_id);
    let object = fixture_object(tenant_id, connection.connection_uid);
    let version = fixture_version(object.object_uid);
    let chunk_text = format!(
        "Safe introduction for the object. {} {RAW_DOCUMENT_TAIL}",
        "x".repeat(180)
    );
    let chunk = KnowledgeChunk {
        chunk_uid: Uuid::now_v7(),
        version_uid: version.version_uid,
        graph_node_uid: Some(Uuid::now_v7()),
        chunk_hash: "chunk-hash".to_string(),
        block_hashes: vec!["block-hash".to_string()],
        text: chunk_text.clone(),
        heading_path: vec!["Runbook".to_string(), "Rotation".to_string()],
        ordinal: 0,
        token_count: 42,
        metadata: json!({
            "safe": "chunk",
            "authorization": SECRET_BEARER,
            "nested": { "access_token": SECRET_TOKEN }
        }),
    };
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    repository
        .insert_connection(connection)
        .expect("fixture connection should be inserted");
    repository
        .insert_object_inspection(object.clone(), version, vec![chunk])
        .expect("fixture object inspection should be inserted");
    let service = fixture_service(
        repository,
        Arc::new(FakeLinkedIntegrationProvider::default()),
        48,
    );

    let list = service
        .list_objects(KnowledgeObjectListRequest {
            tenant_id,
            connection_uid: None,
            object_type: None,
            cursor: None,
            limit: Some(10),
        })
        .await
        .expect("object list should be rendered");
    let inspect = service
        .inspect_object(KnowledgeObjectInspectRequest {
            tenant_id,
            object_uid: object.object_uid,
        })
        .await
        .expect("object inspection should be rendered");
    let list_json = serde_json::to_string(&list).expect("list response should serialize");
    let inspect_json = serde_json::to_string(&inspect).expect("inspect response should serialize");

    assert_eq!(list.objects.len(), 1);
    assert_eq!(inspect.chunks.len(), 1);
    assert!(inspect.preview.as_deref().unwrap_or("").len() <= 51);
    assert!(inspect.chunks[0].preview.len() <= 51);
    assert!(inspect.chunks[0].preview.ends_with("..."));
    assert!(!list_json.contains(SECRET_TOKEN));
    assert!(!list_json.contains(SECRET_BEARER));
    assert!(!inspect_json.contains(SECRET_TOKEN));
    assert!(!inspect_json.contains(SECRET_BEARER));
    assert!(!inspect_json.contains(RAW_DOCUMENT_TAIL));
    assert!(!inspect_json.contains(&chunk_text));
}

#[tokio::test]
async fn query_trace_is_present_and_does_not_hydrate_cross_contact_memory() {
    // Pins: Task 8 keeps query_trace as a protected surface without leaking unrelated memory.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let service = fixture_service(
        Arc::new(InMemoryKnowledgeRepository::default()),
        Arc::new(FakeLinkedIntegrationProvider::default()),
        80,
    );

    let response = service
        .query_trace(KnowledgeQueryTraceRequest {
            tenant_id,
            trace_uid: Uuid::now_v7(),
        })
        .await
        .expect("query trace should return a renderer-safe placeholder");
    let response_json =
        serde_json::to_string(&response).expect("query trace response should serialize");

    assert!(response.hits.is_empty());
    assert!(response.stages.is_empty());
    assert!(response.searched_scopes.is_empty());
    assert!(!response_json.contains(OTHER_CONTACT_MEMORY));
}

#[test]
fn knowledge_handlers_are_authorized_or_have_webhook_safety_comment() {
    // Pins: tenant-data Knowledge handlers keep authz at the Restate boundary.
    let source = include_str!("../src/services/knowledge/mod.rs");
    let impl_source = source
        .split("impl Knowledge for KnowledgeImpl")
        .nth(1)
        .expect("KnowledgeImpl implementation should exist");

    for method in [
        "create_link_token",
        "exchange_public_token",
        "sync_connection",
        "sync_status",
        "sync_events",
        "list_connections",
        "list_objects",
        "inspect_object",
        "query_trace",
    ] {
        let body = handler_body(impl_source, method);
        let authz = body
            .find("authorize_tenant(&ctx, request.tenant_id).await?;")
            .unwrap_or_else(|| panic!("{method} should authorize tenant access"));
        let service = body
            .find("production_service")
            .unwrap_or_else(|| panic!("{method} should use production service after authz"));
        assert!(
            authz < service,
            "{method} should authorize before constructing service work"
        );
    }

    let webhook_body = handler_body(impl_source, "provider_webhook");
    assert!(
        webhook_body.contains("// SAFETY: Provider webhooks do not carry caller auth;")
            && webhook_body.contains("provider adapters verify the raw signature"),
        "provider_webhook should carry a SAFETY comment explaining signature verification"
    );
    assert!(
        webhook_body.find("provider_webhook(request)").is_some(),
        "provider_webhook should delegate only after signature-verifying service logic is selected"
    );
}

fn fixture_service(
    repository: Arc<dyn KnowledgeRepository>,
    provider: Arc<dyn LinkedIntegrationProvider>,
    max_preview_chars: usize,
) -> KnowledgeService {
    KnowledgeService::new(
        repository,
        Arc::new(StaticKnowledgeProviders::new().with_provider(PROVIDER, provider)),
        Arc::new(FakeKnowledgeCredentialStore::default()),
        max_preview_chars,
    )
}

fn fixture_connection(tenant_id: TenantId) -> KnowledgeConnection {
    KnowledgeConnection {
        connection_uid: Uuid::now_v7(),
        tenant_id,
        provider: PROVIDER.to_string(),
        connector: CONNECTOR.to_string(),
        provider_account_id: "provider-account-1".to_string(),
        credential_ref: "vault://existing".to_string(),
        status: ConnectionStatus::Active,
        metadata: json!({ "safe": "connection" }),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        last_synced_at: None,
    }
}

fn fixture_object(tenant_id: TenantId, connection_uid: Uuid) -> KnowledgeObject {
    KnowledgeObject {
        object_uid: Uuid::now_v7(),
        tenant_id,
        connection_uid,
        object_type: "document".to_string(),
        source_id: "doc-1".to_string(),
        parent_source_id: None,
        source_uri: Some("https://example.test/doc-1".to_string()),
        title: Some("Rotation Runbook".to_string()),
        change_token: Some("etag-1".to_string()),
        metadata: json!({
            "safe": "object",
            "access_token": SECRET_TOKEN,
            "nested": { "authorization": SECRET_BEARER }
        }),
        status: ObjectStatus::Active,
        source_updated_at: Some(Utc::now()),
        deleted_at: None,
    }
}

fn fixture_version(object_uid: Uuid) -> DocumentVersion {
    DocumentVersion {
        version_uid: Uuid::now_v7(),
        object_uid,
        parser: "native".to_string(),
        parser_job_id: Some("job-1".to_string()),
        content_hash: "content-hash".to_string(),
        metadata: json!({
            "safe": "version",
            "refresh_token": SECRET_TOKEN
        }),
        created_at: Utc::now(),
    }
}

fn webhook_request(
    tenant_id: TenantId,
    connection_uid: Uuid,
    event_id: &str,
) -> KnowledgeProviderWebhookRequest {
    let payload = json!({
        "tenant_id": tenant_id.to_string(),
        "connection_uid": connection_uid.to_string(),
        "event_id": event_id,
        "event_type": "sync.completed"
    });
    KnowledgeProviderWebhookRequest {
        provider: PROVIDER.to_string(),
        event_id: event_id.to_string(),
        event_type: "sync.completed".to_string(),
        payload,
        headers: vec![("x-test-signature".to_string(), "valid".to_string())],
        body_base64: None,
    }
}

fn handler_body<'a>(source: &'a str, method: &str) -> &'a str {
    let needle = format!("async fn {method}");
    let method_start = source
        .find(&needle)
        .unwrap_or_else(|| panic!("{method} handler should exist"));
    let start = source[..method_start]
        .rfind("    #[tracing::instrument")
        .unwrap_or(method_start);
    let rest = &source[start..];
    let end = rest
        .find("\n    #[tracing::instrument")
        .or_else(|| rest.find("\n/// Application logic"))
        .expect("handler body should be followed by another handler or application logic");
    &rest[..end]
}

#[derive(Debug, Clone, Default)]
struct FakeLinkedIntegrationProvider {
    calls: Arc<Mutex<FakeProviderCalls>>,
}

impl FakeLinkedIntegrationProvider {
    fn trigger_sync_count(&self) -> usize {
        self.calls().trigger_sync
    }

    fn list_changed_records_count(&self) -> usize {
        self.calls().list_changed_records
    }

    fn verify_webhook_count(&self) -> usize {
        self.calls().verify_webhook
    }

    fn exchange_count(&self) -> usize {
        self.calls().exchange_public_token
    }

    fn calls(&self) -> FakeProviderCalls {
        self.calls
            .lock()
            .expect("fake provider call log should not be poisoned")
            .clone()
    }
}

#[derive(Debug, Clone, Default)]
struct FakeProviderCalls {
    exchange_public_token: usize,
    trigger_sync: usize,
    list_changed_records: usize,
    verify_webhook: usize,
}

#[async_trait]
impl LinkedIntegrationProvider for FakeLinkedIntegrationProvider {
    async fn create_link_token(
        &self,
        _req: CreateLinkTokenRequest,
    ) -> moa_knowledge::Result<LinkToken> {
        Ok(LinkToken {
            provider: PROVIDER.to_string(),
            token: "link-token".to_string(),
            link_url: Some("https://provider.example/link".to_string()),
            expires_at: None,
        })
    }

    async fn exchange_public_token(
        &self,
        _req: ExchangePublicTokenRequest,
    ) -> moa_knowledge::Result<LinkedAccount> {
        self.calls
            .lock()
            .expect("fake provider call log should not be poisoned")
            .exchange_public_token += 1;
        Ok(LinkedAccount {
            provider: PROVIDER.to_string(),
            connector: CONNECTOR.to_string(),
            provider_account_id: "provider-account-1".to_string(),
            credential_ref: SECRET_TOKEN.to_string(),
            metadata: json!({
                "safe": "account",
                "access_token": SECRET_TOKEN
            }),
        })
    }

    async fn trigger_sync(&self, req: TriggerSyncRequest) -> moa_knowledge::Result<TriggeredSync> {
        self.calls
            .lock()
            .expect("fake provider call log should not be poisoned")
            .trigger_sync += 1;
        Ok(TriggeredSync {
            provider: PROVIDER.to_string(),
            provider_sync_id: Some(format!("sync-{}", req.connection.connection_uid)),
            status: "accepted".to_string(),
            metadata: json!({ "accepted": true }),
        })
    }

    async fn list_changed_records(
        &self,
        _req: ListChangedRecordsRequest,
    ) -> moa_knowledge::Result<RecordPage> {
        self.calls
            .lock()
            .expect("fake provider call log should not be poisoned")
            .list_changed_records += 1;
        Ok(RecordPage {
            records: Vec::new(),
            next_cursor: None,
        })
    }

    async fn verify_webhook(
        &self,
        _headers: HeaderMap,
        body: Bytes,
    ) -> moa_knowledge::Result<WebhookEvent> {
        self.calls
            .lock()
            .expect("fake provider call log should not be poisoned")
            .verify_webhook += 1;
        let value: Value = serde_json::from_slice(&body)
            .map_err(|error| KnowledgeError::provider(PROVIDER, error.to_string()))?;
        Ok(WebhookEvent {
            provider: PROVIDER.to_string(),
            event_id: required_string(&value, "event_id")?,
            event_type: required_string(&value, "event_type")?,
            metadata: value,
        })
    }
}

#[derive(Debug, Clone, Default)]
struct FakeKnowledgeCredentialStore {
    accounts: Arc<Mutex<Vec<LinkedAccount>>>,
}

impl FakeKnowledgeCredentialStore {
    fn stored_account_count(&self) -> usize {
        self.accounts
            .lock()
            .expect("fake credential store should not be poisoned")
            .len()
    }

    fn vault_ref_for(&self, tenant_id: TenantId) -> String {
        format!("vault://tenant/{tenant_id}/knowledge/{PROVIDER}/provider-account-1")
    }
}

#[async_trait]
impl KnowledgeCredentialStore for FakeKnowledgeCredentialStore {
    async fn store_linked_account(
        &self,
        tenant_id: TenantId,
        account: &LinkedAccount,
    ) -> Result<String, moa_orchestrator::services::knowledge::KnowledgeServiceError> {
        self.accounts
            .lock()
            .expect("fake credential store should not be poisoned")
            .push(account.clone());
        Ok(self.vault_ref_for(tenant_id))
    }
}

#[derive(Debug, Clone, Default)]
struct InMemoryKnowledgeRepository {
    state: Arc<Mutex<RepositoryState>>,
}

impl InMemoryKnowledgeRepository {
    fn insert_connection(&self, connection: KnowledgeConnection) -> moa_knowledge::Result<()> {
        self.with_state(|state| {
            state
                .connections
                .insert(connection.connection_uid, connection);
        })
    }

    fn insert_object_inspection(
        &self,
        object: KnowledgeObject,
        version: DocumentVersion,
        chunks: Vec<KnowledgeChunk>,
    ) -> moa_knowledge::Result<()> {
        self.with_state(|state| {
            state.versions.insert(version.object_uid, version.clone());
            state.chunks.insert(version.version_uid, chunks);
            state.objects.insert(object.object_uid, object);
        })
    }

    fn connection(&self, connection_uid: Uuid) -> Option<KnowledgeConnection> {
        self.state
            .lock()
            .expect("repository state should not be poisoned")
            .connections
            .get(&connection_uid)
            .cloned()
    }

    fn op_count(&self, op: &'static str) -> usize {
        self.state
            .lock()
            .expect("repository state should not be poisoned")
            .op_counts
            .get(op)
            .copied()
            .unwrap_or(0)
    }

    fn sync_run_count(&self) -> usize {
        self.state
            .lock()
            .expect("repository state should not be poisoned")
            .sync_runs
            .len()
    }

    fn step_count(&self) -> usize {
        self.state
            .lock()
            .expect("repository state should not be poisoned")
            .steps
            .len()
    }

    fn provider_event_count(&self) -> usize {
        self.state
            .lock()
            .expect("repository state should not be poisoned")
            .provider_events
            .len()
    }

    fn record_op(&self, op: &'static str) -> moa_knowledge::Result<()> {
        self.with_state(|state| {
            *state.op_counts.entry(op).or_insert(0) += 1;
        })
    }

    fn with_state<T>(
        &self,
        apply: impl FnOnce(&mut RepositoryState) -> T,
    ) -> moa_knowledge::Result<T> {
        self.state
            .lock()
            .map_err(|error| {
                KnowledgeError::Repository(format!("repository mutex poisoned: {error}"))
            })
            .map(|mut state| apply(&mut state))
    }
}

#[derive(Debug, Default)]
struct RepositoryState {
    connections: HashMap<Uuid, KnowledgeConnection>,
    sync_runs: HashMap<Uuid, KnowledgeSyncRun>,
    steps: Vec<KnowledgeIngestionStep>,
    objects: HashMap<Uuid, KnowledgeObject>,
    versions: HashMap<Uuid, DocumentVersion>,
    chunks: HashMap<Uuid, Vec<KnowledgeChunk>>,
    provider_events: HashMap<(TenantId, String, String), KnowledgeProviderEventRecord>,
    op_counts: HashMap<&'static str, usize>,
}

#[async_trait]
impl KnowledgeRepository for InMemoryKnowledgeRepository {
    async fn upsert_connection(
        &self,
        connection: KnowledgeConnection,
    ) -> moa_knowledge::Result<()> {
        self.record_op("upsert_connection")?;
        self.with_state(|state| {
            state
                .connections
                .insert(connection.connection_uid, connection);
        })
    }

    async fn get_connection(
        &self,
        connection_uid: Uuid,
    ) -> moa_knowledge::Result<Option<KnowledgeConnection>> {
        self.record_op("get_connection")?;
        self.with_state(|state| state.connections.get(&connection_uid).cloned())
    }

    async fn list_connections(
        &self,
        tenant_id: TenantId,
        provider: Option<&str>,
    ) -> moa_knowledge::Result<Vec<KnowledgeConnectionProjection>> {
        self.record_op("list_connections")?;
        self.with_state(|state| {
            state
                .connections
                .values()
                .filter(|connection| connection.tenant_id == tenant_id)
                .filter(|connection| {
                    provider.is_none_or(|provider| provider == connection.provider)
                })
                .cloned()
                .map(|connection| {
                    let last_sync_status = state
                        .sync_runs
                        .values()
                        .filter(|run| run.connection_uid == connection.connection_uid)
                        .max_by_key(|run| run.started_at)
                        .map(|run| run.status);
                    KnowledgeConnectionProjection {
                        connection,
                        last_sync_status,
                    }
                })
                .collect()
        })
    }

    async fn create_sync_run(&self, run: KnowledgeSyncRun) -> moa_knowledge::Result<()> {
        self.record_op("create_sync_run")?;
        self.with_state(|state| {
            state.sync_runs.insert(run.sync_run_uid, run);
        })
    }

    async fn get_sync_run(
        &self,
        sync_run_uid: Uuid,
    ) -> moa_knowledge::Result<Option<KnowledgeSyncRun>> {
        self.record_op("get_sync_run")?;
        self.with_state(|state| state.sync_runs.get(&sync_run_uid).cloned())
    }

    async fn update_sync_run(&self, run: KnowledgeSyncRun) -> moa_knowledge::Result<()> {
        self.record_op("update_sync_run")?;
        self.with_state(|state| {
            state.sync_runs.insert(run.sync_run_uid, run);
        })
    }

    async fn add_sync_counters(
        &self,
        sync_run_uid: Uuid,
        counters: KnowledgeSyncCounters,
    ) -> moa_knowledge::Result<()> {
        self.record_op("add_sync_counters")?;
        self.with_state(|state| {
            if let Some(run) = state.sync_runs.get_mut(&sync_run_uid) {
                run.records_seen += counters.records_seen;
                run.records_ingested += counters.records_ingested;
                run.records_failed += counters.records_failed;
            }
        })
    }

    async fn record_ingestion_step(
        &self,
        step: KnowledgeIngestionStep,
    ) -> moa_knowledge::Result<()> {
        self.record_op("record_ingestion_step")?;
        self.with_state(|state| {
            state.steps.push(step);
        })
    }

    async fn sync_run_steps(
        &self,
        sync_run_uid: Uuid,
        object_uid: Option<Uuid>,
    ) -> moa_knowledge::Result<Vec<KnowledgeIngestionStep>> {
        self.record_op("sync_run_steps")?;
        self.with_state(|state| {
            let mut steps = state
                .steps
                .iter()
                .filter(|step| step.sync_run_uid == sync_run_uid)
                .filter(|step| {
                    object_uid.is_none_or(|object_uid| step.object_uid == Some(object_uid))
                })
                .cloned()
                .collect::<Vec<_>>();
            steps.sort_by_key(|step| (step.started_at, step.step.clone(), step.retry_count));
            steps
        })
    }

    async fn upsert_object(&self, object: KnowledgeObject) -> moa_knowledge::Result<()> {
        self.record_op("upsert_object")?;
        self.with_state(|state| {
            state.objects.insert(object.object_uid, object);
        })
    }

    async fn get_object(&self, object_uid: Uuid) -> moa_knowledge::Result<Option<KnowledgeObject>> {
        self.record_op("get_object")?;
        self.with_state(|state| state.objects.get(&object_uid).cloned())
    }

    async fn list_objects(
        &self,
        tenant_id: TenantId,
        connection_uid: Option<Uuid>,
        object_type: Option<&str>,
        limit: u32,
    ) -> moa_knowledge::Result<Vec<KnowledgeObjectProjection>> {
        self.record_op("list_objects")?;
        self.with_state(|state| {
            state
                .objects
                .values()
                .filter(|object| object.tenant_id == tenant_id)
                .filter(|object| {
                    connection_uid
                        .is_none_or(|connection_uid| object.connection_uid == connection_uid)
                })
                .filter(|object| {
                    object_type.is_none_or(|object_type| object.object_type == object_type)
                })
                .take(limit as usize)
                .cloned()
                .map(|object| {
                    let version = state.versions.get(&object.object_uid);
                    let chunks = version
                        .and_then(|version| state.chunks.get(&version.version_uid))
                        .cloned()
                        .unwrap_or_default();
                    KnowledgeObjectProjection {
                        parser: version.map(|version| version.parser.clone()),
                        parser_status: if version.is_some() {
                            "parsed".to_string()
                        } else {
                            "pending".to_string()
                        },
                        chunk_count: chunks.len() as u64,
                        graph_node_count: chunks
                            .iter()
                            .filter(|chunk| chunk.graph_node_uid.is_some())
                            .count() as u64,
                        object,
                    }
                })
                .collect()
        })
    }

    async fn get_object_by_source(
        &self,
        connection_uid: Uuid,
        source_id: &str,
    ) -> moa_knowledge::Result<Option<KnowledgeObject>> {
        self.record_op("get_object_by_source")?;
        self.with_state(|state| {
            state
                .objects
                .values()
                .find(|object| {
                    object.connection_uid == connection_uid && object.source_id == source_id
                })
                .cloned()
        })
    }

    async fn latest_document_version(
        &self,
        object_uid: Uuid,
    ) -> moa_knowledge::Result<Option<DocumentVersion>> {
        self.record_op("latest_document_version")?;
        self.with_state(|state| state.versions.get(&object_uid).cloned())
    }

    async fn chunks_for_version(
        &self,
        version_uid: Uuid,
    ) -> moa_knowledge::Result<Vec<KnowledgeChunk>> {
        self.record_op("chunks_for_version")?;
        self.with_state(|state| state.chunks.get(&version_uid).cloned().unwrap_or_default())
    }

    async fn inspect_object(
        &self,
        object_uid: Uuid,
    ) -> moa_knowledge::Result<Option<KnowledgeObjectInspection>> {
        self.record_op("inspect_object")?;
        self.with_state(|state| {
            let object = state.objects.get(&object_uid)?.clone();
            let version = state.versions.get(&object_uid).cloned();
            let chunks = version
                .as_ref()
                .and_then(|version| state.chunks.get(&version.version_uid))
                .cloned()
                .unwrap_or_default();
            let steps = state
                .steps
                .iter()
                .filter(|step| step.object_uid == Some(object_uid))
                .cloned()
                .collect();
            Some(KnowledgeObjectInspection {
                object,
                version,
                chunks,
                steps,
            })
        })
    }

    async fn insert_document_version(&self, version: DocumentVersion) -> moa_knowledge::Result<()> {
        self.record_op("insert_document_version")?;
        self.with_state(|state| {
            state.versions.insert(version.object_uid, version);
        })
    }

    async fn replace_blocks(
        &self,
        _version_uid: Uuid,
        _blocks: Vec<KnowledgeBlock>,
    ) -> moa_knowledge::Result<()> {
        self.record_op("replace_blocks")
    }

    async fn replace_chunks(
        &self,
        version_uid: Uuid,
        chunks: Vec<KnowledgeChunk>,
    ) -> moa_knowledge::Result<()> {
        self.record_op("replace_chunks")?;
        self.with_state(|state| {
            state.chunks.insert(version_uid, chunks);
        })
    }

    async fn set_chunk_graph_uid(
        &self,
        chunk_uid: Uuid,
        graph_node_uid: Uuid,
    ) -> moa_knowledge::Result<()> {
        self.record_op("set_chunk_graph_uid")?;
        self.with_state(|state| {
            for chunks in state.chunks.values_mut() {
                if let Some(chunk) = chunks.iter_mut().find(|chunk| chunk.chunk_uid == chunk_uid) {
                    chunk.graph_node_uid = Some(graph_node_uid);
                }
            }
        })
    }

    async fn tombstone_chunks(&self, _chunk_uids: &[Uuid]) -> moa_knowledge::Result<()> {
        self.record_op("tombstone_chunks")
    }

    async fn mark_object_deleted(
        &self,
        object_uid: Uuid,
        deleted_at: chrono::DateTime<chrono::Utc>,
    ) -> moa_knowledge::Result<()> {
        self.record_op("mark_object_deleted")?;
        self.with_state(|state| {
            if let Some(object) = state.objects.get_mut(&object_uid) {
                object.status = ObjectStatus::Deleted;
                object.deleted_at = Some(deleted_at);
            }
        })
    }

    async fn upsert_contact_group(&self, _group: ContactGroup) -> moa_knowledge::Result<()> {
        self.record_op("upsert_contact_group")
    }

    async fn replace_contact_group_memberships(
        &self,
        _group_uid: Uuid,
        _memberships: Vec<ContactGroupMembership>,
    ) -> moa_knowledge::Result<()> {
        self.record_op("replace_contact_group_memberships")
    }

    async fn record_provider_event(
        &self,
        event: KnowledgeProviderEventRecord,
    ) -> moa_knowledge::Result<KnowledgeProviderEventRecord> {
        self.record_op("record_provider_event")?;
        self.with_state(|state| {
            let key = (
                event.tenant_id,
                event.provider.clone(),
                event.provider_event_id.clone(),
            );
            if let Some(existing) = state.provider_events.get(&key) {
                let mut duplicate = existing.clone();
                duplicate.duplicate = true;
                return duplicate;
            }
            state.provider_events.insert(key, event.clone());
            event
        })
    }
}

fn required_string(value: &Value, field: &str) -> moa_knowledge::Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| KnowledgeError::provider(PROVIDER, format!("missing `{field}`")))
}

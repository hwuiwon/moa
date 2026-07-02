//! Offline coverage for the tenant Knowledge service application surface.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use moa_core::RlsContext;
use moa_core::{
    ContactId, SessionId, StoragePartitionId, TenantId, UserId,
    traits::EmbeddingProvider,
    wire::knowledge::{
        KnowledgeExchangeTokenRequest, KnowledgeIntegrationListRequest,
        KnowledgeObjectInspectRequest, KnowledgeObjectListRequest, KnowledgeProviderWebhookRequest,
        KnowledgeQueryTraceRequest, KnowledgeSyncEventsRequest, KnowledgeSyncRequest,
        KnowledgeSyncStatusRequest, KnowledgeUpdateConnectionSourceSelectionRequest,
    },
};
use moa_db::ScopedConn;
use moa_knowledge::{
    Error as KnowledgeError,
    chunking::ChunkingConfig,
    contact_groups::derive_contact_groups_from_object_with_resolved_members,
    domain::{
        ApplySourceSelectionRequest, ConnectionStatus, ContactGroup, ContactGroupMembership,
        ContactGroupTarget, CreateLinkTokenRequest, DocumentElement, DocumentElementKind,
        DocumentVersion, ElementLayout, ExchangePublicTokenRequest, KnowledgeBlock, KnowledgeChunk,
        KnowledgeConnection, KnowledgeConnectionProjection, KnowledgeIngestionStep,
        KnowledgeObject, KnowledgeObjectInspection, KnowledgeObjectProjection,
        KnowledgeProviderEventRecord, KnowledgeSyncCounters, KnowledgeSyncRun, LinkToken,
        LinkedAccount, ListChangedRecordsRequest, ObjectStatus, ParseInput, ParsedDocument,
        ProviderIntegration, ProviderRecord, RecordPage, SyncRunStatus, TriggerSyncRequest,
        TriggeredSync, WebhookEvent,
    },
    ingestion::{
        KnowledgeIngestionPipeline, KnowledgeIngestionPipelineConfig, MemoryKnowledgeGraphWriter,
        PageIngestionReport,
    },
    observability::MetricsIngestionObserver,
    parser::DocumentParser,
    providers::LinkedIntegrationProvider,
    repository::{
        KnowledgeRepository, PostgresKnowledgeRepository, ProviderAccountConnectionLookup,
    },
};
use moa_lineage_core::{
    BackendIntrospection, FusedHit, GraphPath, LineageEvent, RecordKind, RerankHit,
    RetrievalLineage, RetrievalSelectedHit, RetrievalStage, StageTimings, TurnId, VecHit,
};
use moa_memory_graph::{GraphStore, NodeLabel, NodeWriteIntent, PiiClass, PostgresGraphStore};
use moa_memory_types::MemoryScope;
use moa_memory_vector::{PgvectorStore, VECTOR_DIMENSION};
use moa_orchestrator::services::knowledge::{
    KnowledgeCredentialStore, KnowledgeIngestionRunner, KnowledgeService, KnowledgeServiceError,
    KnowledgeWebhookVerifier, ParserWebhookVerifier, StaticKnowledgeProviders,
};
use moa_orchestrator::workflows::knowledge_sync_ingestion::{
    KnowledgeSyncIngestionDurableSteps, KnowledgeSyncIngestionRequest,
    KnowledgeSyncPageApplication, KnowledgeSyncPreparedRun, KnowledgeSyncProviderPage,
    run_knowledge_sync_ingestion_workflow,
};
use reqwest::header::HeaderMap;
use restate_sdk::prelude::{HandlerError, TerminalError};
use serde_json::{Value, json};
use sha2::Sha256;
use tokio_util::bytes::Bytes;
use uuid::Uuid;

const PROVIDER: &str = "fake";
const CONNECTOR: &str = "drive";
const SECRET_TOKEN: &str = "provider-secret-token-123";
const SECRET_BEARER: &str = "Bearer provider-secret-token-456";
const RAW_DOCUMENT_TAIL: &str = "RAW_FULL_DOCUMENT_TAIL_SHOULD_NOT_APPEAR";

#[tokio::test]
async fn knowledge_auto_sync_manual_sync_triggers_provider_and_does_not_ingest_inline() {
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

    assert_eq!(response.status, "provider_syncing");
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
async fn knowledge_auto_sync_manual_sync_immediate_provider_completion_marks_run_ready_for_workflow()
 {
    // Pins: immediate provider completion marks the run provider-synced and records the same ingestion enqueue marker used by webhooks.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection = fixture_connection(tenant_id);
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    repository
        .insert_connection(connection.clone())
        .expect("fixture connection should be inserted");
    let provider = Arc::new(FakeLinkedIntegrationProvider::with_trigger_status(
        "completed",
    ));
    let service = fixture_service(repository.clone(), provider.clone(), 80);

    let response = service
        .sync_connection(KnowledgeSyncRequest {
            tenant_id,
            connection_uid: connection.connection_uid,
            parser: Some("native".to_string()),
            max_records: Some(25),
        })
        .await
        .expect("manual sync should accept an immediate provider completion");

    assert_eq!(response.status, "provider_synced");
    assert_eq!(provider.trigger_sync_count(), 1);
    assert_eq!(provider.list_changed_records_count(), 0);
    assert_eq!(repository.op_count("create_sync_run"), 1);
    assert_eq!(repository.op_count("update_sync_run"), 1);
    assert_eq!(repository.op_count("record_ingestion_step"), 2);
    assert_eq!(repository.sync_run_count(), 1);
    assert_eq!(repository.step_count(), 2);
}

#[tokio::test]
async fn knowledge_auto_sync_duplicate_webhook_does_not_double_dispatch_or_count() {
    // Pins: duplicate provider deliveries are idempotent and enqueue ingestion only once.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection =
        fixture_connection_for_provider(tenant_id, "nango", "google-drive", "provider-account-1");
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    repository
        .insert_connection(connection.clone())
        .expect("fixture connection should be inserted");
    let service = fixture_webhook_service(repository.clone(), "nango", 80);
    let request = signed_connection_webhook_request(
        "nango",
        tenant_id,
        connection.connection_uid,
        "evt-duplicate",
        "sync:completed",
    );

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
    assert_eq!(repository.provider_event_count(), 1);
    assert_eq!(repository.sync_run_count(), 1);
    assert_eq!(repository.step_count(), 1);
}

#[tokio::test]
async fn knowledge_auto_sync_provider_webhook_dispatches_once_offline() {
    // Pins: Merge linked_account.synced is an enabled provider completion signal.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection =
        fixture_connection_for_provider(tenant_id, "merge", "merge", "linked-account-123");
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    repository
        .insert_connection(connection.clone())
        .expect("fixture Merge connection should be inserted");
    let service = fixture_webhook_service(repository.clone(), "merge", 80);
    let request = signed_provider_webhook_request(
        "merge",
        json!({
            "event_id": "evt-merge-synced",
            "event_type": "linked_account.synced",
            "linked_account": { "id": "linked-account-123" }
        }),
    );

    let response = service
        .provider_webhook(request)
        .await
        .expect("Merge synced webhook should enqueue ingestion");

    assert!(response.ingestion_enqueued);
    assert!(response.sync_run_uid.is_some());
    assert_eq!(repository.provider_event_count(), 1);
    assert_eq!(repository.sync_run_count(), 1);
    assert_eq!(repository.step_count(), 1);
}

#[tokio::test]
async fn knowledge_auto_sync_distinct_events_reuse_active_connection_run() {
    // Pins: distinct completion events for one connection do not create parallel active runs.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection =
        fixture_connection_for_provider(tenant_id, "nango", "google-drive", "provider-account-1");
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    repository
        .insert_connection(connection.clone())
        .expect("fixture Nango connection should be inserted");
    let service = fixture_webhook_service(repository.clone(), "nango", 80);
    let first_request = signed_connection_webhook_request(
        "nango",
        tenant_id,
        connection.connection_uid,
        "evt-nango-sync-completed",
        "sync.completed",
    );
    let second_request = signed_connection_webhook_request(
        "nango",
        tenant_id,
        connection.connection_uid,
        "evt-nango-sync-colon-completed",
        "sync:completed",
    );

    let first = service
        .provider_webhook(first_request)
        .await
        .expect("first Nango completion should enqueue ingestion");
    let second = service
        .provider_webhook(second_request)
        .await
        .expect("second Nango completion should reuse the active run");

    assert!(first.ingestion_enqueued);
    assert!(!second.duplicate);
    assert!(!second.ingestion_enqueued);
    assert_eq!(second.sync_run_uid, first.sync_run_uid);
    assert_eq!(repository.provider_event_count(), 2);
    assert_eq!(repository.sync_run_count(), 1);
    assert_eq!(repository.step_count(), 1);
    assert_eq!(repository.op_count("create_sync_run"), 1);
}

#[tokio::test]
async fn non_sync_provider_webhook_is_stored_without_enqueueing() {
    // Pins: unrelated provider events are persisted for audit but do not start ingestion.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection =
        fixture_connection_for_provider(tenant_id, "nango", "google-drive", "provider-account-1");
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    repository
        .insert_connection(connection.clone())
        .expect("fixture Nango connection should be inserted");
    let service = fixture_webhook_service(repository.clone(), "nango", 80);
    let request = signed_connection_webhook_request(
        "nango",
        tenant_id,
        connection.connection_uid,
        "evt-nango-connection-updated",
        "connection.updated",
    );

    let response = service
        .provider_webhook(request)
        .await
        .expect("non-sync provider event should be recorded");

    assert!(!response.duplicate);
    assert!(!response.ingestion_enqueued);
    assert!(response.sync_run_uid.is_none());
    assert_eq!(repository.provider_event_count(), 1);
    assert_eq!(repository.sync_run_count(), 0);
    assert_eq!(repository.step_count(), 0);
}

#[tokio::test]
async fn list_integrations_merges_providers_sorted_and_honors_provider_filter() {
    // Pins: connect UIs get every enabled provider's integrations, provider-tagged
    // and deterministically sorted, and an explicit provider filter narrows the list.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let nango_like = Arc::new(FakeLinkedIntegrationProvider::with_integrations(vec![
        ProviderIntegration {
            id: "notion".to_string(),
            display_name: "Notion".to_string(),
            logo_url: None,
        },
        ProviderIntegration {
            id: "google-drive".to_string(),
            display_name: "Google Drive".to_string(),
            logo_url: Some("https://logos.example/drive.png".to_string()),
        },
    ]));
    let merge_like = Arc::new(FakeLinkedIntegrationProvider::with_integrations(vec![
        ProviderIntegration {
            id: "filestorage".to_string(),
            display_name: "File Storage".to_string(),
            logo_url: None,
        },
    ]));
    let broken_like = Arc::new(FakeLinkedIntegrationProvider::with_integrations_error(
        "catalog endpoint returned 500",
    ));
    let service = KnowledgeService::new(
        Arc::new(InMemoryKnowledgeRepository::default()),
        Arc::new(
            StaticKnowledgeProviders::new()
                .with_provider("nango", nango_like)
                .with_provider("merge", merge_like)
                .with_provider("broken", broken_like),
        ),
        Arc::new(FakeKnowledgeCredentialStore::default()),
        fake_ingestion_runner(),
        80,
    );

    let all = service
        .list_integrations(KnowledgeIntegrationListRequest {
            tenant_id,
            provider: None,
        })
        .await
        .expect("list integrations across providers");
    let flattened: Vec<(String, String)> = all
        .integrations
        .iter()
        .map(|entry| (entry.provider.clone(), entry.id.clone()))
        .collect();
    assert_eq!(
        flattened,
        vec![
            ("merge".to_string(), "filestorage".to_string()),
            ("nango".to_string(), "google-drive".to_string()),
            ("nango".to_string(), "notion".to_string()),
        ],
        "integrations should be sorted by provider then integration id"
    );
    assert_eq!(
        all.integrations[1].logo_url.as_deref(),
        Some("https://logos.example/drive.png")
    );
    assert_eq!(
        all.unavailable_providers.len(),
        1,
        "a failing enabled provider must be reported, not silently dropped"
    );
    assert_eq!(all.unavailable_providers[0].provider, "broken");
    assert!(
        all.unavailable_providers[0]
            .reason
            .contains("catalog endpoint returned 500"),
        "reason should carry the provider failure message"
    );

    let filtered = service
        .list_integrations(KnowledgeIntegrationListRequest {
            tenant_id,
            provider: Some("merge".to_string()),
        })
        .await
        .expect("list integrations for one provider");
    assert_eq!(filtered.integrations.len(), 1);
    assert_eq!(filtered.integrations[0].provider, "merge");
    assert_eq!(filtered.integrations[0].id, "filestorage");

    let unknown = service
        .list_integrations(KnowledgeIntegrationListRequest {
            tenant_id,
            provider: Some("unknown".to_string()),
        })
        .await;
    assert!(
        unknown.is_err(),
        "explicit unknown provider filter must surface an error"
    );

    let broken = service
        .list_integrations(KnowledgeIntegrationListRequest {
            tenant_id,
            provider: Some("broken".to_string()),
        })
        .await;
    assert!(
        broken.is_err(),
        "explicit provider filter must propagate the provider failure"
    );
}

#[tokio::test]
async fn provider_webhook_resolves_signed_provider_account_identity() {
    // Pins: signed provider account metadata resolves the local connection without tenant fields.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let mut connection = fixture_connection(tenant_id);
    connection.provider = "nango".to_string();
    connection.connector = "google-drive".to_string();
    connection.provider_account_id = "conn_123".to_string();
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    repository
        .insert_connection(connection.clone())
        .expect("fixture Nango connection should be inserted");
    let service = KnowledgeService::new(
        repository.clone(),
        Arc::new(
            StaticKnowledgeProviders::new()
                .with_webhook_verifier("nango", Arc::new(PayloadWebhookVerifier::new("nango"))),
        ),
        Arc::new(FakeKnowledgeCredentialStore::default()),
        fake_ingestion_runner(),
        80,
    );
    let request = signed_provider_webhook_request(
        "nango",
        json!({
            "event_id": "evt-provider-account",
            "event_type": "sync.completed",
            "connection_id": "conn_123",
            "provider_config_key": "google-drive"
        }),
    );

    let response = service
        .provider_webhook(request)
        .await
        .expect("signed provider account webhook should resolve");
    let stored = repository
        .provider_event(tenant_id, "nango", "evt-provider-account")
        .expect("resolved provider event should be stored");

    assert!(response.ingestion_enqueued);
    assert!(response.sync_run_uid.is_some());
    assert_eq!(stored.connection_uid, Some(connection.connection_uid));
    assert_eq!(
        repository.op_count("lookup_connection_by_provider_account"),
        1
    );
    assert_eq!(repository.provider_event_count(), 1);
    assert_eq!(repository.sync_run_count(), 1);
    assert_eq!(repository.step_count(), 1);
}

#[tokio::test]
async fn provider_webhook_rejects_ambiguous_provider_account_before_recording() {
    // Pins: provider account webhooks fail closed when more than one local row matches.
    let first_tenant = TenantId::from(Uuid::now_v7());
    let second_tenant = TenantId::from(Uuid::now_v7());
    let mut first = fixture_connection(first_tenant);
    first.provider = "merge".to_string();
    first.connector = "merge".to_string();
    first.provider_account_id = "linked-account-123".to_string();
    let mut second = fixture_connection(second_tenant);
    second.provider = "merge".to_string();
    second.connector = "merge".to_string();
    second.provider_account_id = "linked-account-123".to_string();
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    repository
        .insert_connection(first)
        .expect("first Merge connection should be inserted");
    repository
        .insert_connection(second)
        .expect("second Merge connection should be inserted");
    let service = KnowledgeService::new(
        repository.clone(),
        Arc::new(
            StaticKnowledgeProviders::new()
                .with_webhook_verifier("merge", Arc::new(PayloadWebhookVerifier::new("merge"))),
        ),
        Arc::new(FakeKnowledgeCredentialStore::default()),
        fake_ingestion_runner(),
        80,
    );
    let request = signed_provider_webhook_request(
        "merge",
        json!({
            "event_id": "evt-ambiguous",
            "event_type": "linked_account.synced",
            "linked_account": { "id": "linked-account-123" }
        }),
    );

    let error = service
        .provider_webhook(request)
        .await
        .expect_err("ambiguous provider account should be rejected");

    assert!(error.to_string().contains("multiple knowledge connections"));
    assert_eq!(
        repository.op_count("lookup_connection_by_provider_account"),
        1
    );
    assert_eq!(repository.provider_event_count(), 0);
    assert_eq!(repository.sync_run_count(), 0);
    assert_eq!(repository.step_count(), 0);
}

#[tokio::test]
async fn provider_webhook_rejects_missing_verified_binding_before_recording() {
    // Pins: unsigned request payload fields are ignored for webhook tenant binding.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection = fixture_connection(tenant_id);
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    repository
        .insert_connection(connection.clone())
        .expect("fixture connection should be inserted");
    let service = KnowledgeService::new(
        repository.clone(),
        Arc::new(StaticKnowledgeProviders::new().with_webhook_verifier(
            "nango",
            Arc::new(FixedWebhookVerifier::new(WebhookEvent {
                provider: "nango".to_string(),
                event_id: "evt-missing-binding".to_string(),
                event_type: "sync.completed".to_string(),
                metadata: json!({ "safe": "verified but unbound" }),
            })),
        )),
        Arc::new(FakeKnowledgeCredentialStore::default()),
        fake_ingestion_runner(),
        80,
    );
    let request = signed_provider_webhook_request(
        "nango",
        json!({
            "event_id": "evt-missing-binding",
            "event_type": "sync.completed",
            "tenant_id": tenant_id.to_string(),
            "connection_uid": connection.connection_uid.to_string()
        }),
    );

    let error = service
        .provider_webhook(request)
        .await
        .expect_err("missing verified binding should be rejected");

    assert!(error.to_string().contains("provider account binding"));
    assert_eq!(repository.provider_event_count(), 0);
    assert_eq!(repository.sync_run_count(), 0);
    assert_eq!(repository.step_count(), 0);
}

#[tokio::test]
async fn provider_webhook_rejects_signed_connection_for_different_provider_before_recording() {
    // Pins: signed tenant/connection UUID binding must still match the verified provider.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let mut connection = fixture_connection(tenant_id);
    connection.provider = "merge".to_string();
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    repository
        .insert_connection(connection.clone())
        .expect("fixture connection should be inserted");
    let provider = Arc::new(FakeLinkedIntegrationProvider::default());
    let service = fixture_service(repository.clone(), provider, 80);
    let request = webhook_request(
        tenant_id,
        connection.connection_uid,
        "evt-provider-mismatch",
    );

    let error = service
        .provider_webhook(request)
        .await
        .expect_err("signed connection for a different provider should fail");

    assert!(error.to_string().contains("knowledge connection not found"));
    assert_eq!(repository.provider_event_count(), 0);
    assert_eq!(repository.sync_run_count(), 0);
    assert_eq!(repository.step_count(), 0);
}

#[tokio::test]
async fn knowledge_auto_sync_parser_webhook_rejects_bad_signature_and_stores_redacted_metadata() {
    // Pins: parser webhook HMAC verification is fakeable and persists only safe event metadata.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    let mut connection = fixture_connection(tenant_id);
    connection.connection_uid = connection_uid;
    repository
        .insert_connection(connection)
        .expect("seed signed parser webhook connection");
    let verifier = Arc::new(
        ParserWebhookVerifier::new("llamaparse").with_signing_key("llamaparse-webhook-secret"),
    );
    let service = KnowledgeService::new(
        repository.clone(),
        Arc::new(StaticKnowledgeProviders::new().with_webhook_verifier("llamaparse", verifier)),
        Arc::new(FakeKnowledgeCredentialStore::default()),
        fake_ingestion_runner(),
        80,
    );
    let payload = parser_webhook_payload(tenant_id, connection_uid, "lp-job-1");
    let bad_request = parser_webhook_request(
        "llamaparse",
        payload.clone(),
        vec![(
            "x-llamaparse-webhook-signature".to_string(),
            "sha256=bad-signature".to_string(),
        )],
    );
    let good_request = parser_webhook_request(
        "llamaparse",
        payload,
        vec![(
            "x-llamaparse-webhook-signature".to_string(),
            format!(
                "sha256={}",
                webhook_signature_hex("llamaparse-webhook-secret", &bad_request.payload)
            ),
        )],
    );

    let bad_error = service
        .provider_webhook(bad_request)
        .await
        .expect_err("bad parser webhook signature should be rejected");
    let response = service
        .provider_webhook(good_request)
        .await
        .expect("valid parser webhook signature should be accepted");
    let stored = repository
        .provider_event(tenant_id, "llamaparse", "lp-job-1")
        .expect("verified parser event should be stored");
    let stored_json =
        serde_json::to_string(&stored.payload).expect("stored payload should serialize");

    assert!(bad_error.to_string().contains("signature"));
    assert_eq!(response.provider, "llamaparse");
    assert_eq!(response.event_id, "lp-job-1");
    assert!(!response.ingestion_enqueued);
    assert_eq!(
        stored.connection_uid,
        Some(connection_uid),
        "verified metadata should preserve connection_uid"
    );
    assert_eq!(
        stored.payload.get("tenant_id").and_then(Value::as_str),
        Some(tenant_id.to_string().as_str())
    );
    assert!(!stored_json.contains(SECRET_TOKEN));
    assert!(!stored_json.contains(RAW_DOCUMENT_TAIL));
    assert!(!stored_json.contains("raw_document_text"));
    assert_eq!(repository.provider_event_count(), 1);
    assert_eq!(repository.sync_run_count(), 0);
    assert_eq!(repository.step_count(), 0);
}

#[tokio::test]
async fn knowledge_auto_sync_parser_webhook_rejects_bad_custom_header_and_accepts_good_header() {
    // Pins: parser webhook custom-header verification is fakeable without provider API calls.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    let mut connection = fixture_connection(tenant_id);
    connection.connection_uid = connection_uid;
    repository
        .insert_connection(connection)
        .expect("seed signed parser webhook connection");
    let verifier = Arc::new(
        ParserWebhookVerifier::new("reducto")
            .with_custom_header("x-reducto-webhook-secret", "expected-header-secret"),
    );
    let service = KnowledgeService::new(
        repository.clone(),
        Arc::new(StaticKnowledgeProviders::new().with_webhook_verifier("reducto", verifier)),
        Arc::new(FakeKnowledgeCredentialStore::default()),
        fake_ingestion_runner(),
        80,
    );
    let payload = parser_webhook_payload(tenant_id, connection_uid, "reducto-job-1");
    let bad_request = parser_webhook_request(
        "reducto",
        payload.clone(),
        vec![(
            "x-reducto-webhook-secret".to_string(),
            "wrong-header-secret".to_string(),
        )],
    );
    let good_request = parser_webhook_request(
        "reducto",
        payload,
        vec![(
            "x-reducto-webhook-secret".to_string(),
            "expected-header-secret".to_string(),
        )],
    );

    let bad_error = service
        .provider_webhook(bad_request)
        .await
        .expect_err("bad parser webhook custom header should be rejected");
    let response = service
        .provider_webhook(good_request)
        .await
        .expect("valid parser webhook custom header should be accepted");
    let stored = repository
        .provider_event(tenant_id, "reducto", "reducto-job-1")
        .expect("verified parser event should be stored");

    assert!(bad_error.to_string().contains("header"));
    assert_eq!(response.provider, "reducto");
    assert_eq!(response.event_id, "reducto-job-1");
    assert_eq!(stored.connection_uid, Some(connection_uid));
    assert!(!response.ingestion_enqueued);
    assert_eq!(repository.provider_event_count(), 1);
    assert_eq!(repository.sync_run_count(), 0);
    assert_eq!(repository.step_count(), 0);
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
        fake_ingestion_runner(),
        80,
    );

    let response = service
        .exchange_public_token(KnowledgeExchangeTokenRequest {
            tenant_id,
            provider: PROVIDER.to_string(),
            exchange_token: "public-token".to_string(),
            source_selection: json!({
                "metadata": {
                    "selected_folder_ids": ["folder-1"]
                }
            }),
        })
        .await
        .expect("token exchange should persist a connection");
    let connection = repository
        .connection(response.connection_uid)
        .expect("connection should be stored");

    assert_eq!(provider.exchange_count(), 1);
    assert_eq!(provider.apply_source_selection_count(), 1);
    assert_eq!(provider.trigger_sync_count(), 1);
    assert_eq!(response.sync_status.as_deref(), Some("provider_syncing"));
    assert_eq!(repository.sync_run_count(), 1);
    assert_eq!(
        provider.applied_source_selections(),
        vec![json!({ "metadata": { "selected_folder_ids": ["folder-1"] } })]
    );
    assert_eq!(credentials.stored_account_count(), 1);
    assert_eq!(
        connection.credential_ref,
        credentials.vault_ref_for(tenant_id)
    );
    assert_ne!(connection.credential_ref, SECRET_TOKEN);
    assert!(!connection.credential_ref.contains(SECRET_TOKEN));
    assert_eq!(
        connection.source_selection,
        json!({ "metadata": { "selected_folder_ids": ["folder-1"] } })
    );
    assert_eq!(response.provider, PROVIDER);
    assert_eq!(response.connector, CONNECTOR);
}

#[tokio::test]
async fn update_source_selection_persists_applies_and_optionally_syncs() {
    // Pins: tenant admins can update provider-native selected sources and trigger ingestion follow-up.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let mut connection = fixture_connection(tenant_id);
    connection.last_synced_at = Some(Utc::now());
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    repository
        .insert_connection(connection.clone())
        .expect("fixture connection should be inserted");
    let provider = Arc::new(FakeLinkedIntegrationProvider::default());
    let service = fixture_service(repository.clone(), provider.clone(), 80);
    let source_selection = json!({
        "metadata": {
            "selected_folder_ids": ["folder-a", "folder-b"]
        },
        "variant": "selected-sources"
    });

    let response = service
        .update_connection_source_selection(KnowledgeUpdateConnectionSourceSelectionRequest {
            tenant_id,
            connection_uid: connection.connection_uid,
            source_selection: source_selection.clone(),
            sync: true,
        })
        .await
        .expect("source selection update should persist and trigger sync");
    let stored = repository
        .connection(connection.connection_uid)
        .expect("updated connection should stay stored");

    assert_eq!(response.connection_uid, connection.connection_uid);
    assert_eq!(response.source_selection, source_selection);
    assert_eq!(response.sync_status.as_deref(), Some("provider_syncing"));
    assert!(response.sync_run_uid.is_some());
    assert_eq!(stored.source_selection, source_selection);
    assert_eq!(stored.last_synced_at, None);
    assert_eq!(provider.apply_source_selection_count(), 1);
    assert_eq!(
        provider.applied_source_selections(),
        vec![stored.source_selection]
    );
    assert_eq!(provider.trigger_sync_count(), 1);
    assert_eq!(repository.sync_run_count(), 1);
}

#[tokio::test]
async fn knowledge_service_accepts_injected_ingestion_runner_without_global_config() {
    // Pins: service tests can inject a deterministic ingestion runner without reading runtime config.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let sync_run_uid = Uuid::now_v7();
    let runner = Arc::new(FakeKnowledgeIngestionRunner::default());
    let service = KnowledgeService::new(
        Arc::new(InMemoryKnowledgeRepository::default()),
        Arc::new(StaticKnowledgeProviders::new()),
        Arc::new(FakeKnowledgeCredentialStore::default()),
        runner.clone(),
        80,
    );
    let run = KnowledgeSyncRun {
        sync_run_uid,
        tenant_id,
        connection_uid,
        parser: Some("native".to_string()),
        max_records: Some(1),
        status: SyncRunStatus::ProviderSynced,
        records_seen: 0,
        records_changed: 0,
        records_deleted: 0,
        records_ingested: 0,
        records_failed: 0,
        objects_parsed: 0,
        chunks_embedded: 0,
        graph_nodes_upserted: 0,
        graph_edges_upserted: 0,
        error_code: None,
        started_at: Utc::now(),
        finished_at: None,
    };
    let page = RecordPage {
        records: vec![ProviderRecord {
            source_id: "doc-1".to_string(),
            object_type: "document".to_string(),
            title: Some("Doc 1".to_string()),
            source_uri: None,
            change_token: Some("etag-1".to_string()),
            deleted: false,
            source_updated_at: None,
            metadata: json!({}),
            payload: json!({ "text": "hello" }),
        }],
        next_cursor: None,
    };

    let report = service
        .ingestion_runner()
        .ingest_record_page(&run, PROVIDER, page)
        .await
        .expect("deterministic runner should ingest the test page");

    assert_eq!(report.records_listed, 1);
    assert_eq!(
        runner.calls(),
        vec![FakeKnowledgeIngestionCall {
            sync_run_uid,
            connection_uid,
            tenant_id,
            provider: PROVIDER.to_string(),
            records_listed: 1,
        }]
    );
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

    assert!(response.hits.is_empty());
    assert!(response.stages.is_empty());
    assert!(response.searched_scopes.is_empty());
}

#[tokio::test]
async fn query_trace_renders_populated_retrieval_lineage_db_memory() {
    // Pins: query_trace renders persisted retrieval lineage without hydrating unrelated contact memory.
    let db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated query trace DB");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let trace_uid = Uuid::now_v7();
    let turn_id = TurnId(trace_uid);
    let session_id = SessionId::new();
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
    let graph_node_uid = Uuid::now_v7();
    let chunk_uid = Uuid::now_v7();
    let event = LineageEvent::Retrieval(RetrievalLineage {
        turn_id,
        session_id,
        storage_partition_id: storage_partition_id.clone(),
        user_id: UserId::new("query-trace-user"),
        scope: MemoryScope::Tenant { tenant_id },
        ts: Utc::now(),
        query_original: "How do I rotate payroll keys?".to_string(),
        query_expansions: vec!["rotate payroll keys".to_string()],
        vector_hits: vec![VecHit {
            chunk_id: chunk_uid,
            score: 0.91,
            source: "pgvector".to_string(),
            embedder: "test-embedder".to_string(),
            embed_dim: 4,
        }],
        graph_paths: vec![GraphPath {
            start: graph_node_uid,
            end: chunk_uid,
            edges: vec![Uuid::now_v7()],
            labels: vec!["HAS_CHUNK".to_string()],
            length: 1,
            score: 0.82,
        }],
        fusion_scores: vec![FusedHit {
            chunk_id: chunk_uid,
            fused_score: 0.94,
            vector_contribution: 0.5,
            graph_contribution: 0.3,
            lexical_contribution: 0.1,
            fusion_method: "rrf".to_string(),
        }],
        rerank_scores: vec![RerankHit {
            chunk_id: chunk_uid,
            original_index: 0,
            relevance_score: 0.97,
            rerank_model: "noop-reranker".to_string(),
        }],
        top_k: vec![chunk_uid],
        searched_scopes: vec!["tenant_knowledge".to_string(), "user_memory".to_string()],
        selected_hits: vec![RetrievalSelectedHit {
            graph_node_uid,
            chunk_uid: Some(chunk_uid),
            fact_uid: None,
            source_tier: "tenant_knowledge".to_string(),
            label: "Chunk".to_string(),
            title: "Payroll Rotation".to_string(),
            snippet: "Rotate payroll keys through the admin console.".to_string(),
            score: 0.97,
            legs: vec!["vector".to_string(), "graph".to_string()],
            prompt_included: true,
            source_uri: Some("https://kb.example/payroll-rotation".to_string()),
            source_title: Some("Payroll Rotation".to_string()),
            citation: json!({ "chunk_hash": "chunk-hash" }),
        }],
        filters: json!({ "pii_floor": "internal" }),
        timings: StageTimings {
            embed_ms: 3,
            vector_search_ms: 5,
            graph_search_ms: 7,
            lexical_search_ms: 2,
            fusion_ms: 1,
            rerank_ms: 4,
            total_ms: 25,
        },
        introspection: BackendIntrospection::default(),
        stage: RetrievalStage::Single,
    });
    sqlx::query(
        r#"
        INSERT INTO analytics.turn_lineage (
            turn_id,
            session_id,
            user_id,
            storage_partition_id,
            ts,
            tier,
            record_kind,
            payload,
            integrity_hash,
            prev_hash
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, NULL)
        "#,
    )
    .bind(turn_id.0)
    .bind(session_id.0)
    .bind("query-trace-user")
    .bind(storage_partition_id.as_str())
    .bind(Utc::now())
    .bind(1_i16)
    .bind(RecordKind::Retrieval.as_i16())
    .bind(serde_json::to_value(event).expect("retrieval lineage should serialize"))
    .bind(vec![0_u8; 32])
    .execute(&pool)
    .await
    .expect("insert retrieval lineage row");
    let service = KnowledgeService::from_postgres_pool(
        pool,
        Arc::new(StaticKnowledgeProviders::new()),
        Arc::new(FakeKnowledgeCredentialStore::default()),
        fake_ingestion_runner(),
        80,
    );

    let response = service
        .query_trace(KnowledgeQueryTraceRequest {
            tenant_id,
            trace_uid,
        })
        .await
        .expect("query trace should render persisted lineage");
    let stage_names = response
        .stages
        .iter()
        .map(|stage| stage.stage.as_str())
        .collect::<Vec<_>>();

    assert_eq!(response.trace_uid, trace_uid);
    assert_eq!(response.original_query, "How do I rotate payroll keys?");
    assert_eq!(
        response.retrieval_query.as_deref(),
        Some("rotate payroll keys")
    );
    assert_eq!(
        response.searched_scopes,
        vec!["tenant_knowledge".to_string(), "user_memory".to_string()]
    );
    assert_eq!(
        stage_names,
        vec![
            "embed", "vector", "graph", "lexical", "fusion", "reranker", "context"
        ]
    );
    assert_eq!(response.hits.len(), 1);
    assert_eq!(response.hits[0].uid, chunk_uid);
    assert_eq!(response.hits[0].source_tier, "tenant_knowledge");
    assert_eq!(
        response.hits[0].citation["legs"],
        json!(["vector", "graph"])
    );
    assert_eq!(
        response.hits[0].citation["source_uri"],
        json!("https://kb.example/payroll-rotation")
    );
}

#[tokio::test]
async fn mock_connector_end_to_end_db_memory() {
    // Pins: fake Merge and Nango connector syncs can be manually driven through tenant KB ingestion and inspected without external credentials.
    let db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated mock connector DB");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let contact_id = ContactId::new();
    let merge_provider = Arc::new(Task14LinkedIntegrationProvider::new(
        "merge",
        "crm",
        task14_merge_records(),
    ));
    let nango_provider = Arc::new(Task14LinkedIntegrationProvider::new(
        "nango",
        "docs",
        task14_nango_records(),
    ));
    let providers = StaticKnowledgeProviders::new()
        .with_provider("merge", merge_provider.clone())
        .with_provider("nango", nango_provider.clone());
    let service = KnowledgeService::from_postgres_pool(
        pool.clone(),
        Arc::new(providers),
        Arc::new(FakeKnowledgeCredentialStore::default()),
        fake_ingestion_runner(),
        96,
    );

    let merge_connection = service
        .exchange_public_token(KnowledgeExchangeTokenRequest {
            tenant_id,
            provider: "merge".to_string(),
            exchange_token: "merge-public-token".to_string(),
            source_selection: json!({}),
        })
        .await
        .expect("merge link should store one fake connection");
    let nango_connection = service
        .exchange_public_token(KnowledgeExchangeTokenRequest {
            tenant_id,
            provider: "nango".to_string(),
            exchange_token: "nango-public-token".to_string(),
            source_selection: json!({}),
        })
        .await
        .expect("nango link should store one fake connection");
    assert_ne!(
        merge_connection.connection_uid,
        nango_connection.connection_uid
    );

    let merge_sync = service
        .sync_connection(KnowledgeSyncRequest {
            tenant_id,
            connection_uid: merge_connection.connection_uid,
            parser: Some("task14".to_string()),
            max_records: Some(10),
        })
        .await
        .expect("merge manual sync should trigger provider sync");
    let nango_sync = service
        .sync_connection(KnowledgeSyncRequest {
            tenant_id,
            connection_uid: nango_connection.connection_uid,
            parser: Some("task14".to_string()),
            max_records: Some(10),
        })
        .await
        .expect("nango manual sync should trigger provider sync");
    assert_eq!(merge_sync.status, "provider_syncing");
    assert_eq!(nango_sync.status, "provider_syncing");
    assert_eq!(merge_provider.trigger_sync_count(), 1);
    assert_eq!(nango_provider.trigger_sync_count(), 1);

    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope.clone(),
    ));
    seed_task14_embedder_state(&pool, tenant_id).await;
    let vector = Arc::new(PgvectorStore::new_for_app_role(pool.clone(), scope.clone()));
    let graph_store = Arc::new(
        PostgresGraphStore::scoped_for_app_role(pool.clone(), scope.clone())
            .with_vector_store(vector),
    );
    let graph_writer = Arc::new(MemoryKnowledgeGraphWriter::new(
        graph_store.clone(),
        MemoryScope::Tenant { tenant_id },
        "task14-mock-connector",
    ));
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        Arc::new(Task14Parser),
        Arc::new(Task14Embedder),
        graph_writer,
        Arc::new(MetricsIngestionObserver),
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 128,
                max_tokens: 256,
                min_tokens: 1,
            },
            provider: "mock_connector".to_string(),
            parser_label: "task14".to_string(),
        },
    );

    let merge_connection_row = repository
        .get_connection(merge_connection.connection_uid)
        .await
        .expect("read merge connection")
        .expect("merge connection should exist");
    let nango_connection_row = repository
        .get_connection(nango_connection.connection_uid)
        .await
        .expect("read nango connection")
        .expect("nango connection should exist");
    let merge_page = merge_provider
        .list_changed_records(ListChangedRecordsRequest {
            connection: merge_connection_row,
            cursor: None,
            modified_after: None,
            limit: Some(10),
            variant: None,
        })
        .await
        .expect("merge fake provider should return changed records");
    let nango_page = nango_provider
        .list_changed_records(ListChangedRecordsRequest {
            connection: nango_connection_row,
            cursor: None,
            modified_after: None,
            limit: Some(10),
            variant: None,
        })
        .await
        .expect("nango fake provider should return changed records");
    assert_eq!(merge_provider.list_changed_records_count(), 1);
    assert_eq!(nango_provider.list_changed_records_count(), 1);
    assert_eq!(merge_page.records.len(), 3);
    assert_eq!(nango_page.records.len(), 3);

    pipeline
        .ingest_record_page(
            merge_sync.sync_run_uid,
            merge_connection.connection_uid,
            tenant_id,
            merge_page,
        )
        .await
        .expect("merge fake records should ingest");
    pipeline
        .ingest_record_page(
            nango_sync.sync_run_uid,
            nango_connection.connection_uid,
            tenant_id,
            nango_page,
        )
        .await
        .expect("nango fake records should ingest");
    complete_sync_run(&repository, merge_sync.sync_run_uid)
        .await
        .expect("complete merge sync run");
    complete_sync_run(&repository, nango_sync.sync_run_uid)
        .await
        .expect("complete nango sync run");

    let account_object = repository
        .get_object_by_source(merge_connection.connection_uid, "merge-crm-account")
        .await
        .expect("read account object")
        .expect("account object should be ingested");
    let group_delta =
        derive_contact_groups_from_object_with_resolved_members(&account_object, &[contact_id]);
    assert_eq!(group_delta.groups.len(), 1);
    assert_eq!(group_delta.memberships.len(), 1);
    let group = group_delta
        .groups
        .first()
        .expect("group should be derived")
        .clone();
    repository
        .upsert_contact_group(group.clone())
        .await
        .expect("persist derived contact group");
    repository
        .replace_contact_group_memberships(group.group_uid, group_delta.memberships)
        .await
        .expect("persist derived group membership");
    let group_node_uid = create_contact_group_graph_node(&graph_store, tenant_id, &group)
        .await
        .expect("materialize contact group graph node");
    assert_ne!(group_node_uid, Uuid::nil());

    let merge_status = service
        .sync_status(KnowledgeSyncStatusRequest {
            tenant_id,
            sync_run_uid: merge_sync.sync_run_uid,
        })
        .await
        .expect("merge status should render");
    let nango_status = service
        .sync_status(KnowledgeSyncStatusRequest {
            tenant_id,
            sync_run_uid: nango_sync.sync_run_uid,
        })
        .await
        .expect("nango status should render");
    assert_sync_status_counters(&merge_status, 3, 16, 13);
    assert_sync_status_counters(&nango_status, 3, 15, 12);
    assert_eq!(
        merge_status
            .steps
            .iter()
            .take(2)
            .map(|step| step.step.as_str())
            .collect::<Vec<_>>(),
        vec!["provider_triggered", "provider_records_listed"]
    );
    assert_eq!(
        nango_status
            .steps
            .iter()
            .take(2)
            .map(|step| step.step.as_str())
            .collect::<Vec<_>>(),
        vec!["provider_triggered", "provider_records_listed"]
    );

    let objects = service
        .list_objects(KnowledgeObjectListRequest {
            tenant_id,
            connection_uid: None,
            object_type: None,
            cursor: None,
            limit: Some(10),
        })
        .await
        .expect("object summaries should render");
    assert_eq!(objects.objects.len(), 6);
    let mut object_source_ids = objects
        .objects
        .iter()
        .map(|object| {
            assert_eq!(object["parser_status"], json!("parsed"));
            assert_eq!(object["chunk_count"], json!(1));
            assert_eq!(object["graph_node_count"], json!(1));
            object["source_id"]
                .as_str()
                .expect("object summary should include source_id")
                .to_string()
        })
        .collect::<Vec<_>>();
    object_source_ids.sort();
    assert_eq!(
        object_source_ids,
        vec![
            "merge-crm-account",
            "merge-crm-contact",
            "merge-md-handbook",
            "nango-llamaparse-policy",
            "nango-reducto-layout",
            "nango-unstructured-guide",
        ]
    );

    let llama_object = repository
        .get_object_by_source(nango_connection.connection_uid, "nango-llamaparse-policy")
        .await
        .expect("read llama object")
        .expect("llama object should exist");
    let llama_inspect = service
        .inspect_object(KnowledgeObjectInspectRequest {
            tenant_id,
            object_uid: llama_object.object_uid,
        })
        .await
        .expect("llamaparse object should inspect");
    assert_eq!(llama_inspect.parser.as_deref(), Some("llamaparse"));
    assert_eq!(
        llama_inspect.parser_metadata["job_status"],
        json!("completed")
    );
    assert_eq!(llama_inspect.chunks.len(), 1);
    assert!(
        llama_inspect.chunks[0]
            .preview
            .contains("Finance control is")
    );
    assert_eq!(
        llama_inspect
            .steps
            .iter()
            .map(|step| step.step.as_str())
            .collect::<Vec<_>>(),
        object_ingestion_steps()
    );

    let reducto_object = repository
        .get_object_by_source(nango_connection.connection_uid, "nango-reducto-layout")
        .await
        .expect("read reducto object")
        .expect("reducto object should exist");
    let reducto_inspect = service
        .inspect_object(KnowledgeObjectInspectRequest {
            tenant_id,
            object_uid: reducto_object.object_uid,
        })
        .await
        .expect("reducto object should inspect");
    assert_eq!(reducto_inspect.parser.as_deref(), Some("reducto"));
    assert_eq!(
        reducto_inspect.parser_metadata["blocks"][0]["bbox"],
        json!([0.1, 0.2, 0.7, 0.4])
    );

    let trace_uid = Uuid::now_v7();
    let trace_chunk = llama_inspect
        .chunks
        .first()
        .expect("llamaparse object should expose one chunk");
    let trace_graph_uid = trace_chunk
        .graph_node_uid
        .expect("llamaparse chunk should have a graph node");
    let contact_fact_uid = Uuid::now_v7();
    let retrieval_event = LineageEvent::Retrieval(RetrievalLineage {
        turn_id: TurnId(trace_uid),
        session_id: SessionId::new(),
        storage_partition_id: StoragePartitionId::for_tenant(tenant_id),
        user_id: UserId::new(contact_id.to_string()),
        scope: MemoryScope::Contact {
            tenant_id,
            contact_id,
        },
        ts: Utc::now(),
        query_original: "Where is the finance payroll control?".to_string(),
        query_expansions: vec!["finance payroll control".to_string()],
        vector_hits: vec![VecHit {
            chunk_id: trace_graph_uid,
            score: 0.91,
            source: "pgvector".to_string(),
            embedder: "embed-v4.0".to_string(),
            embed_dim: VECTOR_DIMENSION as u16,
        }],
        graph_paths: vec![GraphPath {
            start: trace_graph_uid,
            end: trace_graph_uid,
            edges: Vec::new(),
            labels: vec!["HAS_CHUNK".to_string()],
            length: 0,
            score: 0.88,
        }],
        fusion_scores: vec![
            FusedHit {
                chunk_id: trace_graph_uid,
                fused_score: 0.94,
                vector_contribution: 1.0,
                graph_contribution: 1.0,
                lexical_contribution: 1.0,
                fusion_method: "rrf".to_string(),
            },
            FusedHit {
                chunk_id: contact_fact_uid,
                fused_score: 0.72,
                vector_contribution: 0.0,
                graph_contribution: 0.0,
                lexical_contribution: 1.0,
                fusion_method: "rrf".to_string(),
            },
        ],
        rerank_scores: vec![
            RerankHit {
                chunk_id: trace_graph_uid,
                original_index: 0,
                relevance_score: 0.97,
                rerank_model: "noop".to_string(),
            },
            RerankHit {
                chunk_id: contact_fact_uid,
                original_index: 1,
                relevance_score: 0.76,
                rerank_model: "noop".to_string(),
            },
        ],
        top_k: vec![trace_graph_uid, contact_fact_uid],
        searched_scopes: vec![
            format!("tenant:{tenant_id}:tenant_knowledge"),
            format!("contact:{tenant_id}:{contact_id}:user_memory"),
        ],
        selected_hits: vec![
            RetrievalSelectedHit {
                graph_node_uid: trace_graph_uid,
                chunk_uid: Some(trace_chunk.chunk_uid),
                fact_uid: None,
                source_tier: "tenant_knowledge".to_string(),
                label: "Chunk".to_string(),
                title: "Finance Controls".to_string(),
                snippet: trace_chunk.preview.clone(),
                score: 0.97,
                legs: vec![
                    "vector".to_string(),
                    "graph".to_string(),
                    "lexical".to_string(),
                ],
                prompt_included: true,
                source_uri: Some("https://nango.example.test/docs/finance-controls".to_string()),
                source_title: Some("Finance Controls".to_string()),
                citation: json!({
                    "chunk_hash": trace_chunk.chunk_hash.clone(),
                    "heading_path": trace_chunk.heading_path.clone(),
                    "object_type": "document",
                }),
            },
            RetrievalSelectedHit {
                graph_node_uid: contact_fact_uid,
                chunk_uid: None,
                fact_uid: Some(contact_fact_uid),
                source_tier: "user_memory".to_string(),
                label: "Fact".to_string(),
                title: "Contact preference".to_string(),
                snippet: "Contact prefers payroll reminders before approval.".to_string(),
                score: 0.76,
                legs: vec!["lexical".to_string()],
                prompt_included: true,
                source_uri: None,
                source_title: None,
                citation: json!({}),
            },
        ],
        filters: json!({
            "source_tiers": ["tenant_knowledge", "user_memory"],
            "tenant_knowledge_labels": ["Chunk", "ContactGroup"],
        }),
        timings: StageTimings {
            embed_ms: 2,
            vector_search_ms: 4,
            graph_search_ms: 6,
            lexical_search_ms: 3,
            fusion_ms: 1,
            rerank_ms: 2,
            total_ms: 21,
        },
        introspection: BackendIntrospection::default(),
        stage: RetrievalStage::Single,
    });
    insert_retrieval_lineage_row(&pool, retrieval_event, trace_uid, tenant_id)
        .await
        .expect("persist task14 retrieval lineage");
    let query_trace = service
        .query_trace(KnowledgeQueryTraceRequest {
            tenant_id,
            trace_uid,
        })
        .await
        .expect("query trace should render task14 retrieval lineage");
    assert_eq!(
        query_trace.original_query,
        "Where is the finance payroll control?"
    );
    assert_eq!(
        query_trace.retrieval_query.as_deref(),
        Some("finance payroll control")
    );
    assert_eq!(
        query_trace.searched_scopes,
        vec![
            format!("tenant:{tenant_id}:tenant_knowledge"),
            format!("contact:{tenant_id}:{contact_id}:user_memory"),
        ]
    );
    assert_eq!(
        query_trace
            .stages
            .iter()
            .map(|stage| (
                stage.stage.as_str(),
                stage.candidate_count,
                stage.latency_ms
            ))
            .collect::<Vec<_>>(),
        vec![
            ("embed", 0, 2),
            ("vector", 1, 4),
            ("graph", 1, 6),
            ("lexical", 2, 3),
            ("fusion", 2, 1),
            ("reranker", 2, 2),
            ("context", 2, 21),
        ]
    );
    assert_eq!(query_trace.hits.len(), 2);
    assert_eq!(query_trace.hits[0].source_tier, "tenant_knowledge");
    assert_eq!(
        query_trace.hits[0].citation["chunk_hash"],
        json!(trace_chunk.chunk_hash.clone())
    );
    assert_eq!(
        query_trace.hits[0].citation["legs"],
        json!(["vector", "graph", "lexical"])
    );
    assert_eq!(
        query_trace.hits[0].citation["source_uri"],
        json!("https://nango.example.test/docs/finance-controls")
    );
    assert_eq!(query_trace.hits[1].source_tier, "user_memory");

    let merge_events = service
        .sync_events(KnowledgeSyncEventsRequest {
            tenant_id,
            sync_run_uid: merge_sync.sync_run_uid,
            object_uid: Some(account_object.object_uid),
            cursor: None,
            limit: Some(20),
        })
        .await
        .expect("object sync events should render");
    assert_eq!(
        merge_events
            .events
            .iter()
            .map(|step| step.step.as_str())
            .collect::<Vec<_>>(),
        object_ingestion_steps()
    );

    let label_counts = graph_label_counts(&pool, tenant_id).await;
    assert_eq!(label_counts.get("Source"), Some(&6));
    assert_eq!(label_counts.get("Document"), Some(&6));
    assert_eq!(label_counts.get("Chunk"), Some(&6));
    assert_eq!(label_counts.get("Fact"), Some(&6));
    assert_eq!(label_counts.get("Entity"), Some(&7));
    assert_eq!(label_counts.get("ContactGroup"), Some(&1));
    assert_eq!(chunk_vector_row_count(&pool, tenant_id).await, 6);

    let target = repository
        .contact_group_targets(tenant_id, &group.group_key)
        .await
        .expect("load derived target group")
        .expect("target group should exist");
    assert_eq!(
        target.group.group_key,
        format!(
            "merge:{}:account:acct-task14",
            merge_connection.connection_uid
        )
    );
    assert_eq!(
        target
            .members
            .iter()
            .map(|member| member.contact_id)
            .collect::<Vec<_>>(),
        vec![contact_id]
    );
    assert_eq!(target.active_graph_memberships.len(), 1);
    assert_eq!(target.active_graph_memberships[0].edge_label, "MEMBER_OF");
    assert_eq!(
        target.active_graph_memberships[0].evidence,
        vec![account_object.object_uid]
    );
}

#[tokio::test]
async fn knowledge_auto_sync_provider_synced_run_lists_changed_records_and_ingests_db_memory() {
    // Pins: a provider-synced run lists changed records with its cursor/limit/watermark and applies them to tenant graph/vector knowledge.
    let db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated knowledge auto-sync DB");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let modified_after = Utc::now();
    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope,
    ));
    repository
        .upsert_connection(KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: "nango".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "nango-task14-account".to_string(),
            credential_ref: "vault://knowledge/nango-task14".to_string(),
            status: ConnectionStatus::Active,
            metadata: json!({ "safe": "connection" }),
            source_selection: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_synced_at: Some(modified_after),
        })
        .await
        .expect("seed Nango knowledge connection");
    let sync_run_uid =
        create_provider_synced_run(&repository, tenant_id, connection_uid, Some(2)).await;
    let provider = Arc::new(Task14LinkedIntegrationProvider::new(
        "nango",
        "docs",
        task14_nango_records(),
    ));
    seed_task14_embedder_state(&pool, tenant_id).await;
    let pipeline = task14_ingestion_pipeline(pool.clone(), repository.clone(), tenant_id, "nango");
    let mut steps =
        DbKnowledgeAutoSyncSteps::new(repository.clone(), provider.clone(), pipeline, 2, "task14");

    let report = run_knowledge_sync_ingestion_workflow(
        &mut steps,
        KnowledgeSyncIngestionRequest { sync_run_uid },
    )
    .await
    .expect("provider-synced run should auto-ingest changed records");

    assert_eq!(report.status, "completed");
    assert_eq!(report.records_listed, 2);
    assert_eq!(report.records_applied, 2);
    assert_eq!(report.records_pruned, 0);
    assert_eq!(
        provider.list_changed_record_requests(),
        vec![FakeListChangedRecordsRequest {
            connection_uid,
            cursor: None,
            limit: Some(2),
            modified_after: Some(modified_after),
            variant: None,
        }]
    );

    let run = repository
        .get_sync_run(sync_run_uid)
        .await
        .expect("read completed sync run")
        .expect("completed sync run should exist");
    assert_eq!(run.status, SyncRunStatus::Completed);
    assert_eq!(run.records_seen, 2);
    assert_eq!(run.records_changed, 2);
    assert_eq!(run.records_deleted, 0);
    assert_eq!(run.records_ingested, 2);
    assert_eq!(run.records_failed, 0);
    assert_eq!(run.objects_parsed, 2);
    assert_eq!(run.chunks_embedded, 2);
    assert!(run.graph_nodes_upserted > 0);
    assert!(run.graph_edges_upserted > 0);
    let updated_connection = repository
        .get_connection(connection_uid)
        .await
        .expect("read updated connection")
        .expect("connection should still exist");
    assert!(
        updated_connection.last_synced_at >= Some(modified_after),
        "completion should advance the connection sync watermark"
    );

    let mut source_ids = repository
        .list_objects(tenant_id, Some(connection_uid), None, 10)
        .await
        .expect("list ingested objects")
        .into_iter()
        .map(|object| object.object.source_id)
        .collect::<Vec<_>>();
    source_ids.sort();
    assert_eq!(
        source_ids,
        vec![
            "nango-llamaparse-policy".to_string(),
            "nango-unstructured-guide".to_string(),
        ]
    );
    let steps = repository
        .sync_run_steps(sync_run_uid, None)
        .await
        .expect("read sync steps");
    assert_eq!(
        steps
            .iter()
            .filter(|step| step.step == "provider_records_listed")
            .count(),
        1
    );
    assert_eq!(
        steps
            .iter()
            .filter(|step| step.step == "object_change_checked")
            .count(),
        2
    );
    assert_eq!(
        steps
            .iter()
            .filter(|step| step.step == "graph_upserted")
            .count(),
        2
    );
    let label_counts = graph_label_counts(&pool, tenant_id).await;
    assert_eq!(label_counts.get("Source"), Some(&2_i64));
    assert_eq!(label_counts.get("Document"), Some(&2_i64));
    assert_eq!(label_counts.get("Chunk"), Some(&2_i64));
    assert_eq!(label_counts.get("Fact"), Some(&2_i64));
    assert_eq!(chunk_vector_row_count(&pool, tenant_id).await, 2);
}

#[tokio::test]
async fn knowledge_auto_sync_record_listing_failure_marks_sync_retryable_db_memory() {
    // Pins: provider record-listing failures mark the DB sync run retryable without applying any records.
    let db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated knowledge auto-sync failure DB");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let modified_after = Utc::now();
    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope,
    ));
    repository
        .upsert_connection(KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: "nango".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "nango-task14-account".to_string(),
            credential_ref: "vault://knowledge/nango-task14".to_string(),
            status: ConnectionStatus::Active,
            metadata: json!({ "safe": "connection" }),
            source_selection: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_synced_at: Some(modified_after),
        })
        .await
        .expect("seed Nango knowledge connection");
    let sync_run_uid =
        create_provider_synced_run(&repository, tenant_id, connection_uid, Some(5)).await;
    let provider = Arc::new(Task14LinkedIntegrationProvider::failing_list(
        "nango",
        "docs",
        "upstream listing timeout",
    ));
    let pipeline = task14_ingestion_pipeline(pool.clone(), repository.clone(), tenant_id, "nango");
    let mut steps =
        DbKnowledgeAutoSyncSteps::new(repository.clone(), provider.clone(), pipeline, 2, "task14");

    let error = run_knowledge_sync_ingestion_workflow(
        &mut steps,
        KnowledgeSyncIngestionRequest { sync_run_uid },
    )
    .await
    .expect_err("provider listing failure should stop the workflow");

    assert!(
        handler_error_text(&error).contains("upstream listing timeout"),
        "workflow error should preserve the safe provider failure message"
    );
    assert_eq!(
        provider.list_changed_record_requests(),
        vec![FakeListChangedRecordsRequest {
            connection_uid,
            cursor: None,
            limit: Some(2),
            modified_after: Some(modified_after),
            variant: None,
        }]
    );
    let run = repository
        .get_sync_run(sync_run_uid)
        .await
        .expect("read failed sync run")
        .expect("failed sync run should exist");
    assert_eq!(run.status, SyncRunStatus::FailedRetryable);
    assert_eq!(run.error_code.as_deref(), Some("provider_error_retryable"));
    assert_eq!(run.records_seen, 0);
    assert_eq!(run.records_changed, 0);
    assert_eq!(run.records_ingested, 0);
    assert_eq!(run.records_failed, 1);
    assert!(run.finished_at.is_some());
    assert!(
        repository
            .list_objects(tenant_id, Some(connection_uid), None, 10)
            .await
            .expect("list objects after failed sync")
            .is_empty()
    );
}

#[tokio::test]
async fn knowledge_sync_ingestion_workflow_paginates_caps_and_completes() {
    // Pins: the workflow lists provider pages with cursor/limit state, applies only the capped records, and completes the run.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let sync_run_uid = Uuid::now_v7();
    let modified_after = Utc::now();
    let mut steps = FakeKnowledgeSyncIngestionSteps::new(KnowledgeSyncPreparedRun {
        run: KnowledgeSyncRun {
            sync_run_uid,
            tenant_id,
            connection_uid,
            parser: Some("native".to_string()),
            max_records: Some(3),
            status: SyncRunStatus::ProviderSynced,
            records_seen: 0,
            records_changed: 0,
            records_deleted: 0,
            records_ingested: 0,
            records_failed: 0,
            objects_parsed: 0,
            chunks_embedded: 0,
            graph_nodes_upserted: 0,
            graph_edges_upserted: 0,
            error_code: None,
            started_at: Utc::now(),
            finished_at: None,
        },
        connection: KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: PROVIDER.to_string(),
            connector: CONNECTOR.to_string(),
            provider_account_id: "provider-account-1".to_string(),
            credential_ref: "resolved-provider-token".to_string(),
            status: ConnectionStatus::Active,
            metadata: json!({}),
            source_selection: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_synced_at: Some(modified_after),
        },
        provider: PROVIDER.to_string(),
        parser_label: "native".to_string(),
        page_size: 2,
        max_records: 3,
    })
    .with_pages(vec![
        fake_record_page(&["doc-1", "doc-2"], Some("page-2")),
        fake_record_page(&["doc-3", "doc-4"], Some("page-3")),
    ]);

    let report = run_knowledge_sync_ingestion_workflow(
        &mut steps,
        KnowledgeSyncIngestionRequest { sync_run_uid },
    )
    .await
    .expect("workflow should complete capped pagination");

    assert_eq!(report.status, "completed");
    assert_eq!(report.records_listed, 3);
    assert_eq!(report.records_applied, 3);
    assert_eq!(report.records_pruned, 0);
    assert_eq!(
        steps.status_transitions,
        vec![
            SyncRunStatus::ProviderSynced,
            SyncRunStatus::Ingesting,
            SyncRunStatus::Completed
        ]
    );
    assert_eq!(
        steps.list_calls,
        vec![
            FakeListPageCall {
                cursor: None,
                limit: 2,
                page_index: 0,
                credential_ref: "resolved-provider-token".to_string(),
                modified_after: Some(modified_after),
            },
            FakeListPageCall {
                cursor: Some("page-2".to_string()),
                limit: 1,
                page_index: 1,
                credential_ref: "resolved-provider-token".to_string(),
                modified_after: Some(modified_after),
            },
        ]
    );
    assert_eq!(
        steps.apply_calls,
        vec![
            FakeApplyPageCall {
                page_index: 0,
                source_ids: vec!["doc-1".to_string(), "doc-2".to_string()],
            },
            FakeApplyPageCall {
                page_index: 1,
                source_ids: vec!["doc-3".to_string()],
            },
        ]
    );
    assert!(steps.fail_calls.is_empty());
    assert!(steps.prune_calls.is_empty());
}

#[tokio::test]
async fn knowledge_sync_ingestion_workflow_empty_page_completes_with_zero_counters() {
    // Pins: an empty provider page still runs the page application boundary and completes with zero records.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let sync_run_uid = Uuid::now_v7();
    let mut steps = FakeKnowledgeSyncIngestionSteps::new(fake_prepared_sync_run(
        tenant_id,
        connection_uid,
        sync_run_uid,
        10,
    ))
    .with_pages(vec![fake_record_page(&[], None)]);

    let report = run_knowledge_sync_ingestion_workflow(
        &mut steps,
        KnowledgeSyncIngestionRequest { sync_run_uid },
    )
    .await
    .expect("empty provider page should complete");

    assert_eq!(report.records_listed, 0);
    assert_eq!(report.records_applied, 0);
    assert_eq!(report.records_pruned, 0);
    assert_eq!(steps.list_calls.len(), 1);
    assert_eq!(steps.apply_calls.len(), 1);
    assert_eq!(steps.apply_calls[0].source_ids, Vec::<String>::new());
    assert_eq!(
        steps.prune_calls,
        vec![FakePruneCall {
            source_ids: Vec::new()
        }]
    );
    assert_eq!(
        steps.status_transitions,
        vec![
            SyncRunStatus::ProviderSynced,
            SyncRunStatus::Ingesting,
            SyncRunStatus::Completed
        ]
    );
}

#[tokio::test]
async fn knowledge_sync_ingestion_workflow_prunes_after_full_selection_refresh() {
    // Pins: a full selected-source refresh carries all seen source IDs into one durable prune step.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let sync_run_uid = Uuid::now_v7();
    let mut steps = FakeKnowledgeSyncIngestionSteps::new(fake_prepared_sync_run(
        tenant_id,
        connection_uid,
        sync_run_uid,
        10,
    ))
    .with_pages(vec![fake_record_page(&["doc-b", "doc-a"], None)]);

    let report = run_knowledge_sync_ingestion_workflow(
        &mut steps,
        KnowledgeSyncIngestionRequest { sync_run_uid },
    )
    .await
    .expect("full source selection refresh should complete and prune unseen objects");

    assert_eq!(report.status, "completed");
    assert_eq!(report.records_listed, 2);
    assert_eq!(report.records_applied, 2);
    assert_eq!(
        steps.prune_calls,
        vec![FakePruneCall {
            source_ids: vec!["doc-a".to_string(), "doc-b".to_string()]
        }]
    );
    assert!(steps.fail_calls.is_empty());
}

#[tokio::test]
async fn knowledge_sync_ingestion_workflow_derives_run_identity_and_pages_journal_boundaries() {
    // Pins: the report derives tenant/connection from the stored sync run, and the workflow
    // threads cursors across one list+apply journal step per provider page.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let sync_run_uid = Uuid::now_v7();
    let last_synced_at = Utc::now();
    let mut prepared = fake_prepared_sync_run(tenant_id, connection_uid, sync_run_uid, 10);
    // A prior watermark keeps this an incremental sync so the listing step receives
    // `modified_after` and the exhaustive-prune branch stays inactive.
    prepared.connection.last_synced_at = Some(last_synced_at);
    let mut steps = FakeKnowledgeSyncIngestionSteps::new(prepared).with_pages(vec![
        fake_record_page(&["doc-1", "doc-2"], Some("page-2")),
        fake_record_page(&["doc-3"], None),
    ]);

    let report = run_knowledge_sync_ingestion_workflow(
        &mut steps,
        KnowledgeSyncIngestionRequest { sync_run_uid },
    )
    .await
    .expect("incremental ingestion across two pages should complete");

    // The report identity is derived from the stored run, not from the request alone.
    assert_eq!(report.sync_run_uid, sync_run_uid);
    assert_eq!(report.tenant_id, tenant_id);
    assert_eq!(report.connection_uid, connection_uid);
    assert_eq!(report.status, "completed");
    assert_eq!(report.records_listed, 3);
    assert_eq!(report.records_applied, 3);
    assert_eq!(report.records_pruned, 0);

    // The run transitions ProviderSynced -> Ingesting (prepare) -> Completed (complete).
    assert_eq!(
        steps.status_transitions,
        vec![
            SyncRunStatus::ProviderSynced,
            SyncRunStatus::Ingesting,
            SyncRunStatus::Completed
        ]
    );

    // One listing journal step per page, threading the provider cursor and watermark forward.
    assert_eq!(
        steps.list_calls,
        vec![
            FakeListPageCall {
                cursor: None,
                limit: 10,
                page_index: 0,
                credential_ref: "resolved-provider-token".to_string(),
                modified_after: Some(last_synced_at),
            },
            FakeListPageCall {
                cursor: Some("page-2".to_string()),
                limit: 8,
                page_index: 1,
                credential_ref: "resolved-provider-token".to_string(),
                modified_after: Some(last_synced_at),
            },
        ]
    );

    // One application journal step per page, page-indexed alongside the listing steps.
    assert_eq!(
        steps.apply_calls,
        vec![
            FakeApplyPageCall {
                page_index: 0,
                source_ids: vec!["doc-1".to_string(), "doc-2".to_string()],
            },
            FakeApplyPageCall {
                page_index: 1,
                source_ids: vec!["doc-3".to_string()],
            },
        ]
    );

    // An incremental sync (watermark present) never prunes and never marks the run failed.
    assert!(steps.prune_calls.is_empty());
    assert!(steps.fail_calls.is_empty());
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
        fake_ingestion_runner(),
        max_preview_chars,
    )
}

fn fixture_webhook_service(
    repository: Arc<dyn KnowledgeRepository>,
    provider: &'static str,
    max_preview_chars: usize,
) -> KnowledgeService {
    KnowledgeService::new(
        repository,
        Arc::new(
            StaticKnowledgeProviders::new()
                .with_webhook_verifier(provider, Arc::new(PayloadWebhookVerifier::new(provider))),
        ),
        Arc::new(FakeKnowledgeCredentialStore::default()),
        fake_ingestion_runner(),
        max_preview_chars,
    )
}

fn fake_ingestion_runner() -> Arc<dyn KnowledgeIngestionRunner> {
    Arc::new(FakeKnowledgeIngestionRunner::default())
}

#[derive(Debug)]
struct FakeKnowledgeSyncIngestionSteps {
    prepared: KnowledgeSyncPreparedRun,
    pages: Vec<RecordPage>,
    list_calls: Vec<FakeListPageCall>,
    apply_calls: Vec<FakeApplyPageCall>,
    prune_calls: Vec<FakePruneCall>,
    fail_calls: Vec<FakeFailCall>,
    status_transitions: Vec<SyncRunStatus>,
}

impl FakeKnowledgeSyncIngestionSteps {
    fn new(prepared: KnowledgeSyncPreparedRun) -> Self {
        Self {
            prepared,
            pages: Vec::new(),
            list_calls: Vec::new(),
            apply_calls: Vec::new(),
            prune_calls: Vec::new(),
            fail_calls: Vec::new(),
            status_transitions: Vec::new(),
        }
    }

    fn with_pages(mut self, pages: Vec<RecordPage>) -> Self {
        self.pages = pages;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FakeListPageCall {
    cursor: Option<String>,
    limit: u32,
    page_index: u32,
    credential_ref: String,
    modified_after: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FakeListChangedRecordsRequest {
    connection_uid: Uuid,
    cursor: Option<String>,
    limit: Option<u32>,
    modified_after: Option<DateTime<Utc>>,
    variant: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FakeApplyPageCall {
    page_index: u32,
    source_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FakePruneCall {
    source_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FakeFailCall {
    stage: &'static str,
    error_message: String,
}

#[async_trait]
impl KnowledgeSyncIngestionDurableSteps for FakeKnowledgeSyncIngestionSteps {
    async fn prepare_ingestion_run(
        &mut self,
        request: &KnowledgeSyncIngestionRequest,
    ) -> Result<KnowledgeSyncPreparedRun, HandlerError> {
        if request.sync_run_uid != self.prepared.run.sync_run_uid {
            return Err(TerminalError::new_with_code(404, "sync run mismatch").into());
        }
        self.status_transitions.push(self.prepared.run.status);
        self.prepared.run.status = SyncRunStatus::Ingesting;
        self.status_transitions.push(self.prepared.run.status);
        Ok(self.prepared.clone())
    }

    async fn list_changed_records_page(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
        cursor: Option<String>,
        limit: u32,
        page_index: u32,
    ) -> Result<KnowledgeSyncProviderPage, HandlerError> {
        self.list_calls.push(FakeListPageCall {
            cursor,
            limit,
            page_index,
            credential_ref: prepared.connection.credential_ref.clone(),
            modified_after: prepared.connection.last_synced_at,
        });
        let page = if self.pages.is_empty() {
            RecordPage {
                records: Vec::new(),
                next_cursor: None,
            }
        } else {
            self.pages.remove(0)
        };
        let records_listed = page.records.len() as u64;
        Ok(KnowledgeSyncProviderPage {
            provider: prepared.provider.clone(),
            page,
            records_listed,
        })
    }

    async fn apply_record_page(
        &mut self,
        _prepared: &KnowledgeSyncPreparedRun,
        page: KnowledgeSyncProviderPage,
        page_index: u32,
    ) -> Result<KnowledgeSyncPageApplication, HandlerError> {
        let source_ids = page
            .page
            .records
            .iter()
            .map(|record| record.source_id.clone())
            .collect::<Vec<_>>();
        let records_applied = source_ids.len() as u64;
        self.apply_calls.push(FakeApplyPageCall {
            page_index,
            source_ids,
        });
        Ok(KnowledgeSyncPageApplication {
            records_listed: page.records_listed,
            records_ingested: records_applied,
            records_skipped: 0,
            records_deleted: 0,
            embeddings_created: 0,
            records_applied,
        })
    }

    async fn prune_unseen_objects(
        &mut self,
        _prepared: &KnowledgeSyncPreparedRun,
        seen_source_ids: HashSet<String>,
    ) -> Result<KnowledgeSyncPageApplication, HandlerError> {
        let mut source_ids = seen_source_ids.into_iter().collect::<Vec<_>>();
        source_ids.sort();
        self.prune_calls.push(FakePruneCall { source_ids });
        Ok(KnowledgeSyncPageApplication {
            records_listed: 0,
            records_ingested: 0,
            records_skipped: 0,
            records_deleted: 0,
            embeddings_created: 0,
            records_applied: 0,
        })
    }

    async fn complete_ingestion_run(
        &mut self,
        _prepared: &KnowledgeSyncPreparedRun,
    ) -> Result<(), HandlerError> {
        self.status_transitions.push(SyncRunStatus::Completed);
        Ok(())
    }

    async fn fail_ingestion_run(
        &mut self,
        _prepared: &KnowledgeSyncPreparedRun,
        stage: &'static str,
        error_message: String,
    ) -> Result<(), HandlerError> {
        self.fail_calls.push(FakeFailCall {
            stage,
            error_message,
        });
        Ok(())
    }
}

fn fake_prepared_sync_run(
    tenant_id: TenantId,
    connection_uid: Uuid,
    sync_run_uid: Uuid,
    max_records: u32,
) -> KnowledgeSyncPreparedRun {
    KnowledgeSyncPreparedRun {
        run: KnowledgeSyncRun {
            sync_run_uid,
            tenant_id,
            connection_uid,
            parser: Some("native".to_string()),
            max_records: Some(max_records),
            status: SyncRunStatus::ProviderSynced,
            records_seen: 0,
            records_changed: 0,
            records_deleted: 0,
            records_ingested: 0,
            records_failed: 0,
            objects_parsed: 0,
            chunks_embedded: 0,
            graph_nodes_upserted: 0,
            graph_edges_upserted: 0,
            error_code: None,
            started_at: Utc::now(),
            finished_at: None,
        },
        connection: KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: PROVIDER.to_string(),
            connector: CONNECTOR.to_string(),
            provider_account_id: "provider-account-1".to_string(),
            credential_ref: "resolved-provider-token".to_string(),
            status: ConnectionStatus::Active,
            metadata: json!({}),
            source_selection: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_synced_at: None,
        },
        provider: PROVIDER.to_string(),
        parser_label: "native".to_string(),
        page_size: 100,
        max_records,
    }
}

fn fake_record_page(source_ids: &[&str], next_cursor: Option<&str>) -> RecordPage {
    RecordPage {
        records: source_ids
            .iter()
            .map(|source_id| ProviderRecord {
                source_id: (*source_id).to_string(),
                object_type: "document".to_string(),
                title: Some((*source_id).to_string()),
                source_uri: None,
                change_token: Some(format!("{source_id}-etag")),
                deleted: false,
                source_updated_at: None,
                metadata: json!({}),
                payload: json!({ "text": source_id }),
            })
            .collect(),
        next_cursor: next_cursor.map(ToOwned::to_owned),
    }
}

type Task14KnowledgeIngestionPipeline = KnowledgeIngestionPipeline<
    PostgresKnowledgeRepository,
    Task14Parser,
    Task14Embedder,
    MemoryKnowledgeGraphWriter<PostgresGraphStore>,
    MetricsIngestionObserver,
>;

struct DbKnowledgeAutoSyncSteps {
    repository: Arc<PostgresKnowledgeRepository>,
    provider: Arc<Task14LinkedIntegrationProvider>,
    pipeline: Arc<Task14KnowledgeIngestionPipeline>,
    page_size: u32,
    parser_label: String,
}

impl DbKnowledgeAutoSyncSteps {
    fn new(
        repository: Arc<PostgresKnowledgeRepository>,
        provider: Arc<Task14LinkedIntegrationProvider>,
        pipeline: Arc<Task14KnowledgeIngestionPipeline>,
        page_size: u32,
        parser_label: impl Into<String>,
    ) -> Self {
        Self {
            repository,
            provider,
            pipeline,
            page_size,
            parser_label: parser_label.into(),
        }
    }
}

#[async_trait]
impl KnowledgeSyncIngestionDurableSteps for DbKnowledgeAutoSyncSteps {
    async fn prepare_ingestion_run(
        &mut self,
        request: &KnowledgeSyncIngestionRequest,
    ) -> Result<KnowledgeSyncPreparedRun, HandlerError> {
        let mut run = self
            .repository
            .get_sync_run(request.sync_run_uid)
            .await
            .map_err(test_handler_error)?
            .ok_or_else(|| TerminalError::new_with_code(404, "knowledge sync run not found"))?;
        let connection = self
            .repository
            .get_connection(run.connection_uid)
            .await
            .map_err(test_handler_error)?
            .ok_or_else(|| TerminalError::new_with_code(404, "knowledge connection not found"))?;
        if connection.tenant_id != run.tenant_id || connection.connection_uid != run.connection_uid
        {
            return Err(
                TerminalError::new_with_code(404, "knowledge connection tenant mismatch").into(),
            );
        }
        let max_records = run.max_records.unwrap_or(100);
        run.status = SyncRunStatus::Ingesting;
        run.parser = Some(self.parser_label.clone());
        self.repository
            .update_sync_run(run.clone())
            .await
            .map_err(test_handler_error)?;
        Ok(KnowledgeSyncPreparedRun {
            provider: connection.provider.clone(),
            run,
            connection,
            parser_label: self.parser_label.clone(),
            page_size: self.page_size,
            max_records,
        })
    }

    async fn list_changed_records_page(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
        cursor: Option<String>,
        limit: u32,
        _page_index: u32,
    ) -> Result<KnowledgeSyncProviderPage, HandlerError> {
        let page = self
            .provider
            .list_changed_records(ListChangedRecordsRequest {
                connection: prepared.connection.clone(),
                cursor,
                modified_after: prepared.connection.last_synced_at,
                limit: Some(limit),
                variant: None,
            })
            .await
            .map_err(test_handler_error)?;
        let records_listed = page.records.len() as u64;
        Ok(KnowledgeSyncProviderPage {
            provider: prepared.provider.clone(),
            page,
            records_listed,
        })
    }

    async fn apply_record_page(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
        page: KnowledgeSyncProviderPage,
        _page_index: u32,
    ) -> Result<KnowledgeSyncPageApplication, HandlerError> {
        let report = self
            .pipeline
            .ingest_record_page(
                prepared.run.sync_run_uid,
                prepared.run.connection_uid,
                prepared.run.tenant_id,
                page.page,
            )
            .await
            .map_err(test_handler_error)?;
        Ok(KnowledgeSyncPageApplication::from(report))
    }

    async fn prune_unseen_objects(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
        seen_source_ids: HashSet<String>,
    ) -> Result<KnowledgeSyncPageApplication, HandlerError> {
        let report = self
            .pipeline
            .prune_unseen_objects(
                prepared.run.sync_run_uid,
                prepared.run.connection_uid,
                prepared.run.tenant_id,
                &seen_source_ids,
            )
            .await
            .map_err(test_handler_error)?;
        Ok(KnowledgeSyncPageApplication::from(report))
    }

    async fn complete_ingestion_run(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
    ) -> Result<(), HandlerError> {
        let mut run = self
            .repository
            .get_sync_run(prepared.run.sync_run_uid)
            .await
            .map_err(test_handler_error)?
            .ok_or_else(|| TerminalError::new_with_code(404, "knowledge sync run not found"))?;
        run.status = SyncRunStatus::Completed;
        run.error_code = None;
        run.finished_at = Some(Utc::now());
        self.repository
            .update_sync_run(run)
            .await
            .map_err(test_handler_error)?;

        let mut connection = self
            .repository
            .get_connection(prepared.run.connection_uid)
            .await
            .map_err(test_handler_error)?
            .ok_or_else(|| TerminalError::new_with_code(404, "knowledge connection not found"))?;
        connection.last_synced_at = Some(Utc::now());
        self.repository
            .upsert_connection(connection)
            .await
            .map_err(test_handler_error)?;
        Ok(())
    }

    async fn fail_ingestion_run(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
        stage: &'static str,
        error_message: String,
    ) -> Result<(), HandlerError> {
        let Some(mut run) = self
            .repository
            .get_sync_run(prepared.run.sync_run_uid)
            .await
            .map_err(test_handler_error)?
        else {
            return Ok(());
        };
        let classification = moa_knowledge::observability::classify_failure(
            stage,
            &KnowledgeError::provider(prepared.provider.clone(), error_message),
        );
        run.status = if classification.retryable {
            SyncRunStatus::FailedRetryable
        } else {
            SyncRunStatus::FailedTerminal
        };
        run.records_failed = run.records_failed.saturating_add(1);
        run.error_code = Some(classification.error_code.to_string());
        run.finished_at = Some(Utc::now());
        self.repository
            .update_sync_run(run)
            .await
            .map_err(test_handler_error)?;
        Ok(())
    }
}

async fn create_provider_synced_run(
    repository: &PostgresKnowledgeRepository,
    tenant_id: TenantId,
    connection_uid: Uuid,
    max_records: Option<u32>,
) -> Uuid {
    let sync_run_uid = Uuid::now_v7();
    repository
        .create_sync_run(KnowledgeSyncRun {
            sync_run_uid,
            tenant_id,
            connection_uid,
            parser: Some("task14".to_string()),
            max_records,
            status: SyncRunStatus::ProviderSynced,
            records_seen: 0,
            records_changed: 0,
            records_deleted: 0,
            records_ingested: 0,
            records_failed: 0,
            objects_parsed: 0,
            chunks_embedded: 0,
            graph_nodes_upserted: 0,
            graph_edges_upserted: 0,
            error_code: None,
            started_at: Utc::now(),
            finished_at: None,
        })
        .await
        .expect("create provider-synced sync run");
    sync_run_uid
}

fn task14_ingestion_pipeline(
    pool: sqlx::PgPool,
    repository: Arc<PostgresKnowledgeRepository>,
    tenant_id: TenantId,
    provider: &str,
) -> Arc<Task14KnowledgeIngestionPipeline> {
    let scope = RlsContext::tenant(tenant_id);
    let vector = Arc::new(PgvectorStore::new_for_app_role(pool.clone(), scope.clone()));
    let graph_store = Arc::new(
        PostgresGraphStore::scoped_for_app_role(pool, scope.clone()).with_vector_store(vector),
    );
    let graph_writer = Arc::new(MemoryKnowledgeGraphWriter::new(
        graph_store,
        MemoryScope::Tenant { tenant_id },
        "knowledge-auto-sync-test",
    ));
    Arc::new(KnowledgeIngestionPipeline::new(
        repository,
        Arc::new(Task14Parser),
        Arc::new(Task14Embedder),
        graph_writer,
        Arc::new(MetricsIngestionObserver),
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 128,
                max_tokens: 256,
                min_tokens: 1,
            },
            provider: provider.to_string(),
            parser_label: "task14".to_string(),
        },
    ))
}

fn test_handler_error(error: impl std::fmt::Display) -> HandlerError {
    TerminalError::new(error.to_string()).into()
}

fn handler_error_text(error: &HandlerError) -> String {
    let error_ref = <HandlerError as AsRef<dyn std::error::Error + Send + Sync>>::as_ref(error);
    error_ref.to_string()
}

#[derive(Debug, Clone, Default)]
struct FakeKnowledgeIngestionRunner {
    calls: Arc<Mutex<Vec<FakeKnowledgeIngestionCall>>>,
}

impl FakeKnowledgeIngestionRunner {
    fn calls(&self) -> Vec<FakeKnowledgeIngestionCall> {
        self.calls
            .lock()
            .expect("fake ingestion runner calls should not be poisoned")
            .clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FakeKnowledgeIngestionCall {
    sync_run_uid: Uuid,
    connection_uid: Uuid,
    tenant_id: TenantId,
    provider: String,
    records_listed: u64,
}

#[async_trait]
impl KnowledgeIngestionRunner for FakeKnowledgeIngestionRunner {
    async fn ingest_record_page(
        &self,
        run: &KnowledgeSyncRun,
        provider: &str,
        page: RecordPage,
    ) -> Result<PageIngestionReport, KnowledgeServiceError> {
        let records_listed = page.records.len() as u64;
        self.calls
            .lock()
            .expect("fake ingestion runner calls should not be poisoned")
            .push(FakeKnowledgeIngestionCall {
                sync_run_uid: run.sync_run_uid,
                connection_uid: run.connection_uid,
                tenant_id: run.tenant_id,
                provider: provider.to_string(),
                records_listed,
            });
        Ok(PageIngestionReport {
            records_listed,
            records_ingested: records_listed,
            ..PageIngestionReport::default()
        })
    }

    async fn prune_unseen_objects(
        &self,
        run: &KnowledgeSyncRun,
        provider: &str,
        seen_source_ids: &HashSet<String>,
    ) -> Result<PageIngestionReport, KnowledgeServiceError> {
        self.calls
            .lock()
            .expect("fake ingestion runner calls should not be poisoned")
            .push(FakeKnowledgeIngestionCall {
                sync_run_uid: run.sync_run_uid,
                connection_uid: run.connection_uid,
                tenant_id: run.tenant_id,
                provider: provider.to_string(),
                records_listed: seen_source_ids.len() as u64,
            });
        Ok(PageIngestionReport::default())
    }
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
        source_selection: json!({}),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        last_synced_at: None,
    }
}

fn fixture_connection_for_provider(
    tenant_id: TenantId,
    provider: &str,
    connector: &str,
    provider_account_id: &str,
) -> KnowledgeConnection {
    let mut connection = fixture_connection(tenant_id);
    connection.provider = provider.to_string();
    connection.connector = connector.to_string();
    connection.provider_account_id = provider_account_id.to_string();
    connection
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

fn signed_connection_webhook_request(
    provider: &str,
    tenant_id: TenantId,
    connection_uid: Uuid,
    event_id: &str,
    event_type: &str,
) -> KnowledgeProviderWebhookRequest {
    signed_provider_webhook_request(
        provider,
        json!({
            "tenant_id": tenant_id.to_string(),
            "connection_uid": connection_uid.to_string(),
            "event_id": event_id,
            "event_type": event_type
        }),
    )
}

fn signed_provider_webhook_request(
    provider: &str,
    payload: Value,
) -> KnowledgeProviderWebhookRequest {
    let event_id = payload
        .get("event_id")
        .and_then(Value::as_str)
        .expect("provider webhook fixture should include event_id")
        .to_string();
    let event_type = payload
        .get("event_type")
        .and_then(Value::as_str)
        .expect("provider webhook fixture should include event_type")
        .to_string();
    KnowledgeProviderWebhookRequest {
        provider: provider.to_string(),
        event_id,
        event_type,
        payload,
        headers: vec![("x-test-signature".to_string(), "valid".to_string())],
        body_base64: None,
    }
}

fn parser_webhook_payload(tenant_id: TenantId, connection_uid: Uuid, event_id: &str) -> Value {
    json!({
        "tenant_id": tenant_id.to_string(),
        "connection_uid": connection_uid.to_string(),
        "event_id": event_id,
        "event_type": "sync.completed",
        "metadata": {
            "safe": "parser",
            "access_token": SECRET_TOKEN,
            "raw_document_text": format!("parser document body {RAW_DOCUMENT_TAIL}")
        }
    })
}

fn parser_webhook_request(
    provider: &str,
    payload: Value,
    headers: Vec<(String, String)>,
) -> KnowledgeProviderWebhookRequest {
    let event_id = payload
        .get("event_id")
        .and_then(Value::as_str)
        .expect("parser webhook fixture should include event_id")
        .to_string();
    let event_type = payload
        .get("event_type")
        .and_then(Value::as_str)
        .expect("parser webhook fixture should include event_type")
        .to_string();
    KnowledgeProviderWebhookRequest {
        provider: provider.to_string(),
        event_id,
        event_type,
        payload,
        headers,
        body_base64: None,
    }
}

fn webhook_signature_hex(signing_key: &str, payload: &Value) -> String {
    let body = serde_json::to_vec(payload).expect("parser webhook fixture should serialize");
    let mut mac = Hmac::<Sha256>::new_from_slice(signing_key.as_bytes())
        .expect("parser webhook signing key should be valid");
    mac.update(&body);
    hex::encode(mac.finalize().into_bytes())
}

async fn complete_sync_run(
    repository: &PostgresKnowledgeRepository,
    sync_run_uid: Uuid,
) -> moa_knowledge::Result<()> {
    let Some(mut run) = repository.get_sync_run(sync_run_uid).await? else {
        return Err(KnowledgeError::Repository(format!(
            "missing sync run {sync_run_uid}"
        )));
    };
    run.status = SyncRunStatus::Completed;
    run.finished_at = Some(Utc::now());
    repository.update_sync_run(run).await
}

async fn seed_task14_embedder_state(pool: &sqlx::PgPool, tenant_id: TenantId) {
    let mut conn = ScopedConn::begin_tenant(pool, tenant_id)
        .await
        .expect("begin Task14 embedder state seed transaction");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role for Task14 embedder state seed");
    sqlx::query(
        r#"
        INSERT INTO moa.storage_partition_state
            (storage_partition_id, embedding_model, embedding_model_version, embedding_dimension)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (storage_partition_id) DO UPDATE
            SET embedding_model = EXCLUDED.embedding_model,
                embedding_model_version = EXCLUDED.embedding_model_version,
                embedding_dimension = EXCLUDED.embedding_dimension,
                reembed_state = 'steady'
        "#,
    )
    .bind(StoragePartitionId::for_tenant(tenant_id).to_string())
    .bind(TASK14_EMBEDDING_MODEL)
    .bind(TASK14_EMBEDDING_MODEL_VERSION)
    .bind(VECTOR_DIMENSION as i32)
    .execute(conn.as_mut())
    .await
    .expect("seed Task14 storage partition embedder state");
    conn.commit()
        .await
        .expect("commit Task14 embedder state seed");
}

async fn insert_retrieval_lineage_row(
    pool: &sqlx::PgPool,
    event: LineageEvent,
    trace_uid: Uuid,
    tenant_id: TenantId,
) -> sqlx::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO analytics.turn_lineage (
            turn_id,
            session_id,
            user_id,
            storage_partition_id,
            ts,
            tier,
            record_kind,
            payload,
            integrity_hash,
            prev_hash
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, NULL)
        "#,
    )
    .bind(trace_uid)
    .bind(SessionId::new().0)
    .bind("task14-contact")
    .bind(StoragePartitionId::for_tenant(tenant_id).to_string())
    .bind(Utc::now())
    .bind(1_i16)
    .bind(RecordKind::Retrieval.as_i16())
    .bind(serde_json::to_value(event).expect("retrieval lineage should serialize"))
    .bind(vec![0_u8; 32])
    .execute(pool)
    .await
    .map(|_| ())
}

fn assert_sync_status_counters(
    status: &moa_core::wire::knowledge::KnowledgeSyncStatusResponse,
    expected_records: u64,
    expected_graph_nodes: u64,
    expected_graph_edges: u64,
) {
    assert_eq!(status.status, "completed");
    assert_eq!(status.records_seen, expected_records);
    assert_eq!(status.records_changed, expected_records);
    assert_eq!(status.records_deleted, 0);
    assert_eq!(status.records_ingested, expected_records);
    assert_eq!(status.records_failed, 0);
    assert_eq!(status.objects_parsed, expected_records);
    assert_eq!(status.chunks_embedded, expected_records);
    assert_eq!(status.graph_nodes_upserted, expected_graph_nodes);
    assert_eq!(status.graph_edges_upserted, expected_graph_edges);
}

fn object_ingestion_steps() -> Vec<&'static str> {
    vec![
        "object_change_checked",
        "content_fetched",
        "parse_submitted",
        "parse_completed",
        "normalized",
        "blocks_diffed",
        "chunks_diffed",
        "embedded",
        "graph_upserted",
        "vector_indexed",
        "contact_groups_derived",
    ]
}

async fn create_contact_group_graph_node(
    graph: &PostgresGraphStore,
    tenant_id: TenantId,
    group: &ContactGroup,
) -> moa_memory_graph::Result<Uuid> {
    graph
        .create_node(NodeWriteIntent {
            uid: group.group_uid,
            label: NodeLabel::ContactGroup,
            storage_partition_id: Some(tenant_id.to_string()),
            contact_id: None,
            scope: "tenant".to_string(),
            name: group.display_name.clone(),
            properties: json!({
                "group_key": group.group_key,
                "display_name": group.display_name,
            }),
            pii_class: PiiClass::None,
            confidence: Some(0.95),
            valid_from: Utc::now(),
            embedding: None,
            embedding_model: None,
            embedding_model_version: None,
            embedding_text: None,
            actor_id: Uuid::now_v7().to_string(),
            actor_kind: "system".to_string(),
        })
        .await
}

async fn graph_label_counts(pool: &sqlx::PgPool, tenant_id: TenantId) -> HashMap<String, i64> {
    sqlx::query_as::<_, (String, i64)>(
        r#"
        SELECT label::TEXT, count(*)
        FROM moa.node_index
        WHERE storage_partition_id = $1
          AND valid_to IS NULL
        GROUP BY label
        "#,
    )
    .bind(tenant_id.to_string())
    .fetch_all(pool)
    .await
    .expect("read graph label counts")
    .into_iter()
    .collect()
}

async fn chunk_vector_row_count(pool: &sqlx::PgPool, tenant_id: TenantId) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)
        FROM moa.embeddings
        WHERE storage_partition_id = $1
          AND label = 'Chunk'
        "#,
    )
    .bind(tenant_id.to_string())
    .fetch_one(pool)
    .await
    .expect("read chunk vector row count")
}

#[derive(Debug, Clone)]
struct Task14LinkedIntegrationProvider {
    provider: &'static str,
    connector: &'static str,
    records: Arc<Vec<ProviderRecord>>,
    calls: Arc<Mutex<FakeProviderCalls>>,
    list_error: Option<&'static str>,
}

impl Task14LinkedIntegrationProvider {
    fn new(provider: &'static str, connector: &'static str, records: Vec<ProviderRecord>) -> Self {
        Self {
            provider,
            connector,
            records: Arc::new(records),
            calls: Arc::new(Mutex::new(FakeProviderCalls::default())),
            list_error: None,
        }
    }

    fn failing_list(
        provider: &'static str,
        connector: &'static str,
        message: &'static str,
    ) -> Self {
        Self {
            provider,
            connector,
            records: Arc::new(Vec::new()),
            calls: Arc::new(Mutex::new(FakeProviderCalls::default())),
            list_error: Some(message),
        }
    }

    fn trigger_sync_count(&self) -> usize {
        self.calls().trigger_sync
    }

    fn list_changed_records_count(&self) -> usize {
        self.calls().list_changed_records
    }

    fn list_changed_record_requests(&self) -> Vec<FakeListChangedRecordsRequest> {
        self.calls().list_changed_record_requests
    }

    fn calls(&self) -> FakeProviderCalls {
        self.calls
            .lock()
            .expect("task14 fake provider call log should not be poisoned")
            .clone()
    }
}

#[async_trait]
impl LinkedIntegrationProvider for Task14LinkedIntegrationProvider {
    async fn create_link_token(
        &self,
        _req: CreateLinkTokenRequest,
    ) -> moa_knowledge::Result<LinkToken> {
        Ok(LinkToken {
            provider: self.provider.to_string(),
            token: format!("{}-task14-link-token", self.provider),
            link_url: Some(format!("https://{}.example.test/link", self.provider)),
            expires_at: None,
        })
    }

    async fn exchange_public_token(
        &self,
        _req: ExchangePublicTokenRequest,
    ) -> moa_knowledge::Result<LinkedAccount> {
        self.calls
            .lock()
            .expect("task14 fake provider call log should not be poisoned")
            .exchange_public_token += 1;
        Ok(LinkedAccount {
            provider: self.provider.to_string(),
            connector: self.connector.to_string(),
            provider_account_id: format!("{}-task14-account", self.provider),
            credential_ref: format!("{}-account-token", self.provider),
            credential_material: Some(format!("{}-raw-token-should-enter-vault", self.provider)),
            metadata: json!({
                "provider": self.provider,
                "access_token": format!("{}-secret", self.provider),
            }),
        })
    }

    async fn trigger_sync(&self, req: TriggerSyncRequest) -> moa_knowledge::Result<TriggeredSync> {
        self.calls
            .lock()
            .expect("task14 fake provider call log should not be poisoned")
            .trigger_sync += 1;
        Ok(TriggeredSync {
            provider: self.provider.to_string(),
            provider_sync_id: Some(format!(
                "{}-sync-{}",
                self.provider, req.connection.connection_uid
            )),
            status: "accepted".to_string(),
            metadata: json!({ "provider_trigger": "accepted" }),
        })
    }

    async fn list_changed_records(
        &self,
        req: ListChangedRecordsRequest,
    ) -> moa_knowledge::Result<RecordPage> {
        self.calls
            .lock()
            .expect("task14 fake provider call log should not be poisoned")
            .record_list_changed_records_request(&req);
        if let Some(message) = self.list_error {
            return Err(KnowledgeError::provider(self.provider, message));
        }
        let limit = req.limit.unwrap_or(u32::MAX) as usize;
        Ok(RecordPage {
            records: self.records.iter().take(limit).cloned().collect(),
            next_cursor: None,
        })
    }

    async fn verify_webhook(
        &self,
        _headers: HeaderMap,
        _body: Bytes,
    ) -> moa_knowledge::Result<WebhookEvent> {
        self.calls
            .lock()
            .expect("task14 fake provider call log should not be poisoned")
            .verify_webhook += 1;
        Ok(WebhookEvent {
            provider: self.provider.to_string(),
            event_id: format!("{}-task14-webhook", self.provider),
            event_type: "sync.completed".to_string(),
            metadata: json!({ "provider": self.provider }),
        })
    }
}

#[derive(Debug, Default)]
struct Task14Parser;

#[async_trait]
impl DocumentParser for Task14Parser {
    async fn parse(&self, input: ParseInput) -> moa_knowledge::Result<ParsedDocument> {
        match input.object.source_id.as_str() {
            "merge-md-handbook" => Ok(parsed_doc(
                "native",
                None,
                "Benefits Handbook",
                json!({ "job_status": "completed", "format": "markdown" }),
                vec![
                    element(
                        "md-heading-1",
                        DocumentElementKind::Heading,
                        "PTO Policy",
                        vec!["Benefits Handbook", "PTO Policy"],
                        0,
                        None,
                        json!({ "markdown_heading_level": 1 }),
                    ),
                    element(
                        "md-paragraph-1",
                        DocumentElementKind::Paragraph,
                        "PTO policy is standardized for all employees.",
                        vec!["Benefits Handbook", "PTO Policy"],
                        1,
                        None,
                        json!({ "markdown": true }),
                    ),
                    element(
                        "md-list-1",
                        DocumentElementKind::ListItem,
                        "Carryover is capped at five days.",
                        vec!["Benefits Handbook", "PTO Policy"],
                        2,
                        None,
                        json!({ "list_marker": "-" }),
                    ),
                ],
            )),
            "nango-llamaparse-policy" => Ok(parsed_doc(
                "llamaparse",
                Some("lp-task14-job"),
                "Finance Controls",
                json!({
                    "job_status": "completed",
                    "markdown": true,
                    "items": 2,
                    "job_metadata": { "pages": 1 }
                }),
                vec![
                    element(
                        "lp-heading-1",
                        DocumentElementKind::Heading,
                        "Finance Controls",
                        vec!["Finance Controls"],
                        0,
                        None,
                        json!({ "llamaparse_item_type": "heading" }),
                    ),
                    element(
                        "lp-item-1",
                        DocumentElementKind::ListItem,
                        "Finance control is dual approval before payroll export.",
                        vec!["Finance Controls"],
                        1,
                        None,
                        json!({ "llamaparse_item_id": "item-1" }),
                    ),
                ],
            )),
            "nango-unstructured-guide" => Ok(parsed_doc(
                "unstructured",
                Some("unstructured-task14-job"),
                "Support Guide",
                json!({ "job_status": "completed", "element_count": 2 }),
                vec![
                    element(
                        "un-title-1",
                        DocumentElementKind::Heading,
                        "Support Guide",
                        vec!["Support Guide"],
                        0,
                        None,
                        json!({ "unstructured_type": "Title" }),
                    ),
                    element(
                        "un-narrative-1",
                        DocumentElementKind::Paragraph,
                        "Support guide is escalated when billing evidence is missing.",
                        vec!["Support Guide"],
                        1,
                        Some(ElementLayout {
                            x: 12.0,
                            y: 24.0,
                            width: 300.0,
                            height: 90.0,
                            page_width: Some(612.0),
                            page_height: Some(792.0),
                            confidence: Some(0.99),
                        }),
                        json!({ "filename": "support-guide.pdf" }),
                    ),
                ],
            )),
            "nango-reducto-layout" => Ok(parsed_doc(
                "reducto",
                Some("reducto-task14-job"),
                "Warehouse Layout",
                json!({
                    "job_status": "completed",
                    "usage": { "pages": 1 },
                    "studio_link": "https://reducto.example.test/studio/task14",
                    "blocks": [
                        {
                            "type": "paragraph",
                            "bbox": [0.1, 0.2, 0.7, 0.4]
                        }
                    ]
                }),
                vec![element(
                    "reducto-chunk-1",
                    DocumentElementKind::ParserChunk,
                    "Warehouse layout is receiving on the east dock.",
                    vec!["Warehouse Layout"],
                    0,
                    Some(ElementLayout {
                        x: 0.1,
                        y: 0.2,
                        width: 0.6,
                        height: 0.2,
                        page_width: Some(1.0),
                        page_height: Some(1.0),
                        confidence: Some(0.98),
                    }),
                    json!({
                        "blocks": [
                            {
                                "type": "paragraph",
                                "bbox": [0.1, 0.2, 0.7, 0.4]
                            }
                        ]
                    }),
                )],
            )),
            "merge-crm-contact" => Ok(parsed_doc(
                "native",
                None,
                "CRM Contact",
                json!({ "job_status": "completed", "format": "crm_contact" }),
                vec![element(
                    "crm-contact-field-1",
                    DocumentElementKind::Field,
                    "CRM contact is linked to the existing MOA contact.",
                    vec!["CRM Contact"],
                    0,
                    None,
                    json!({ "crm_model": "contact", "moa_contact_linked": true }),
                )],
            )),
            "merge-crm-account" => Ok(parsed_doc(
                "native",
                None,
                "Acme Account",
                json!({ "job_status": "completed", "format": "crm_account" }),
                vec![element(
                    "crm-account-field-1",
                    DocumentElementKind::Field,
                    "Acme account is the enterprise renewal group.",
                    vec!["Acme Account"],
                    0,
                    None,
                    json!({ "crm_model": "account" }),
                )],
            )),
            source_id => Err(KnowledgeError::parser(
                "task14",
                format!("unexpected task14 source id {source_id}"),
            )),
        }
    }
}

fn parsed_doc(
    parser: &str,
    parser_job_id: Option<&str>,
    fallback_title: &str,
    metadata: Value,
    elements: Vec<DocumentElement>,
) -> ParsedDocument {
    let text = elements
        .iter()
        .map(|element| element.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    ParsedDocument {
        parser: parser.to_string(),
        parser_job_id: parser_job_id.map(ToOwned::to_owned),
        text: if text.is_empty() {
            fallback_title.to_string()
        } else {
            text
        },
        elements,
        metadata,
    }
}

fn element(
    element_id: &str,
    kind: DocumentElementKind,
    text: &str,
    heading_path: Vec<&str>,
    ordinal: u32,
    layout: Option<ElementLayout>,
    metadata: Value,
) -> DocumentElement {
    DocumentElement {
        element_id: element_id.to_string(),
        kind,
        text: text.to_string(),
        heading_path: heading_path.into_iter().map(ToOwned::to_owned).collect(),
        ordinal,
        page_number: Some(1),
        layout,
        metadata,
    }
}

#[derive(Debug, Default)]
struct Task14Embedder;

const TASK14_EMBEDDING_MODEL: &str = "embed-v4.0";
const TASK14_EMBEDDING_MODEL_VERSION: i32 = 1;

#[async_trait]
impl EmbeddingProvider for Task14Embedder {
    fn model_id(&self) -> &str {
        TASK14_EMBEDDING_MODEL
    }

    fn dimensions(&self) -> usize {
        VECTOR_DIMENSION
    }

    fn model_version(&self) -> i32 {
        TASK14_EMBEDDING_MODEL_VERSION
    }

    async fn embed(&self, inputs: &[String]) -> moa_core::Result<Vec<Vec<f32>>> {
        Ok(inputs.iter().map(|input| task14_vector(input)).collect())
    }
}

fn task14_vector(input: &str) -> Vec<f32> {
    let mut vector = vec![0.0; VECTOR_DIMENSION];
    for (index, byte) in input.bytes().enumerate() {
        vector[index % VECTOR_DIMENSION] += f32::from(byte) / 255.0;
    }
    vector[0] += 1.0;
    vector
}

fn task14_merge_records() -> Vec<ProviderRecord> {
    vec![
        provider_record(
            "merge-md-handbook",
            "article",
            "Benefits Handbook",
            "https://merge.example.test/kb/benefits",
            "# PTO Policy\n\nPTO policy is standardized for all employees.\n\n- Carryover is capped at five days.",
            json!({ "mime_type": "text/markdown", "merge": { "category": "knowledge" } }),
        ),
        provider_record(
            "merge-crm-contact",
            "crm_contact",
            "CRM Contact",
            "https://merge.example.test/crm/contact/member-a",
            "CRM contact is linked to the existing MOA contact.",
            json!({
                "mime_type": "application/json",
                "merge": {
                    "contact": { "id": "contact-task14", "name": "Member A" },
                    "account": { "id": "acct-task14", "name": "Acme" }
                }
            }),
        ),
        provider_record(
            "merge-crm-account",
            "crm_account",
            "Acme Account",
            "https://merge.example.test/crm/account/acct-task14",
            "Acme account is the enterprise renewal group.",
            json!({
                "mime_type": "application/json",
                "merge": {
                    "account": { "id": "acct-task14", "name": "Acme" },
                    "members": [
                        { "email": "member-a@example.invalid" }
                    ]
                }
            }),
        ),
    ]
}

fn task14_nango_records() -> Vec<ProviderRecord> {
    vec![
        provider_record(
            "nango-llamaparse-policy",
            "document",
            "Finance Controls",
            "https://nango.example.test/docs/finance-controls",
            "Finance control is dual approval before payroll export.",
            json!({ "mime_type": "application/pdf", "parser": "llamaparse" }),
        ),
        provider_record(
            "nango-unstructured-guide",
            "document",
            "Support Guide",
            "https://nango.example.test/docs/support-guide",
            "Support guide is escalated when billing evidence is missing.",
            json!({ "mime_type": "application/pdf", "parser": "unstructured" }),
        ),
        provider_record(
            "nango-reducto-layout",
            "document",
            "Warehouse Layout",
            "https://nango.example.test/docs/warehouse-layout",
            "Warehouse layout is receiving on the east dock.",
            json!({ "mime_type": "application/pdf", "parser": "reducto" }),
        ),
    ]
}

fn provider_record(
    source_id: &str,
    object_type: &str,
    title: &str,
    source_uri: &str,
    text: &str,
    metadata: Value,
) -> ProviderRecord {
    ProviderRecord {
        source_id: source_id.to_string(),
        object_type: object_type.to_string(),
        title: Some(title.to_string()),
        source_uri: Some(source_uri.to_string()),
        change_token: Some(format!("{source_id}-v1")),
        deleted: false,
        source_updated_at: Some(Utc::now()),
        metadata,
        payload: json!({ "text": text }),
    }
}

#[derive(Debug, Clone)]
struct FakeLinkedIntegrationProvider {
    calls: Arc<Mutex<FakeProviderCalls>>,
    trigger_status: String,
    integrations: Vec<ProviderIntegration>,
    integrations_error: Option<String>,
}

impl Default for FakeLinkedIntegrationProvider {
    fn default() -> Self {
        Self {
            calls: Arc::new(Mutex::new(FakeProviderCalls::default())),
            trigger_status: "accepted".to_string(),
            integrations: Vec::new(),
            integrations_error: None,
        }
    }
}

impl FakeLinkedIntegrationProvider {
    fn with_trigger_status(status: impl Into<String>) -> Self {
        Self {
            trigger_status: status.into(),
            ..Self::default()
        }
    }

    fn with_integrations(integrations: Vec<ProviderIntegration>) -> Self {
        Self {
            integrations,
            ..Self::default()
        }
    }

    fn with_integrations_error(message: impl Into<String>) -> Self {
        Self {
            integrations_error: Some(message.into()),
            ..Self::default()
        }
    }

    fn trigger_sync_count(&self) -> usize {
        self.calls().trigger_sync
    }

    fn list_changed_records_count(&self) -> usize {
        self.calls().list_changed_records
    }

    fn exchange_count(&self) -> usize {
        self.calls().exchange_public_token
    }

    fn apply_source_selection_count(&self) -> usize {
        self.calls().apply_source_selection
    }

    fn applied_source_selections(&self) -> Vec<Value> {
        self.calls().source_selection_requests
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
    apply_source_selection: usize,
    trigger_sync: usize,
    list_changed_records: usize,
    verify_webhook: usize,
    list_changed_record_requests: Vec<FakeListChangedRecordsRequest>,
    source_selection_requests: Vec<Value>,
}

impl FakeProviderCalls {
    fn record_list_changed_records_request(&mut self, req: &ListChangedRecordsRequest) {
        self.list_changed_records += 1;
        self.list_changed_record_requests
            .push(FakeListChangedRecordsRequest {
                connection_uid: req.connection.connection_uid,
                cursor: req.cursor.clone(),
                limit: req.limit,
                modified_after: req.modified_after,
                variant: req.variant.clone(),
            });
    }
}

#[derive(Debug, Clone)]
struct PayloadWebhookVerifier {
    provider: &'static str,
}

impl PayloadWebhookVerifier {
    fn new(provider: &'static str) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl KnowledgeWebhookVerifier for PayloadWebhookVerifier {
    async fn verify_webhook(
        &self,
        _headers: HeaderMap,
        body: Bytes,
    ) -> moa_knowledge::Result<WebhookEvent> {
        let value: Value = serde_json::from_slice(&body)
            .map_err(|error| KnowledgeError::provider(self.provider, error.to_string()))?;
        let event_id = value
            .get("event_id")
            .and_then(Value::as_str)
            .ok_or_else(|| KnowledgeError::provider(self.provider, "missing `event_id`"))?;
        let event_type = value
            .get("event_type")
            .and_then(Value::as_str)
            .ok_or_else(|| KnowledgeError::provider(self.provider, "missing `event_type`"))?;
        Ok(WebhookEvent {
            provider: self.provider.to_string(),
            event_id: event_id.to_string(),
            event_type: event_type.to_string(),
            metadata: value,
        })
    }
}

#[derive(Debug, Clone)]
struct FixedWebhookVerifier {
    event: WebhookEvent,
}

impl FixedWebhookVerifier {
    fn new(event: WebhookEvent) -> Self {
        Self { event }
    }
}

#[async_trait]
impl KnowledgeWebhookVerifier for FixedWebhookVerifier {
    async fn verify_webhook(
        &self,
        _headers: HeaderMap,
        _body: Bytes,
    ) -> moa_knowledge::Result<WebhookEvent> {
        Ok(self.event.clone())
    }
}

#[async_trait]
impl LinkedIntegrationProvider for FakeLinkedIntegrationProvider {
    async fn list_integrations(&self) -> moa_knowledge::Result<Vec<ProviderIntegration>> {
        if let Some(message) = &self.integrations_error {
            return Err(KnowledgeError::Provider {
                provider: PROVIDER.to_string(),
                message: message.clone(),
            });
        }
        Ok(self.integrations.clone())
    }

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
            credential_ref: "provider-account-token".to_string(),
            credential_material: Some(SECRET_TOKEN.to_string()),
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
            status: self.trigger_status.clone(),
            metadata: json!({ "status": self.trigger_status.clone() }),
        })
    }

    async fn apply_source_selection(
        &self,
        req: ApplySourceSelectionRequest,
    ) -> moa_knowledge::Result<()> {
        let mut calls = self
            .calls
            .lock()
            .expect("fake provider call log should not be poisoned");
        calls.apply_source_selection += 1;
        calls
            .source_selection_requests
            .push(req.connection.source_selection);
        Ok(())
    }

    async fn list_changed_records(
        &self,
        req: ListChangedRecordsRequest,
    ) -> moa_knowledge::Result<RecordPage> {
        self.calls
            .lock()
            .expect("fake provider call log should not be poisoned")
            .record_list_changed_records_request(&req);
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

    async fn resolve_linked_account(
        &self,
        _tenant_id: TenantId,
        connection: &KnowledgeConnection,
    ) -> Result<String, moa_orchestrator::services::knowledge::KnowledgeServiceError> {
        let accounts = self
            .accounts
            .lock()
            .expect("fake credential store should not be poisoned");
        accounts
            .iter()
            .find(|account| account.provider_account_id == connection.provider_account_id)
            .and_then(|account| account.credential_material.clone())
            .or_else(|| Some(connection.credential_ref.clone()))
            .ok_or_else(|| {
                moa_orchestrator::services::knowledge::KnowledgeServiceError::Credential(
                    "fake credential not found".to_string(),
                )
            })
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

    fn provider_event(
        &self,
        tenant_id: TenantId,
        provider: &str,
        provider_event_id: &str,
    ) -> Option<KnowledgeProviderEventRecord> {
        self.state
            .lock()
            .expect("repository state should not be poisoned")
            .provider_events
            .get(&(
                tenant_id,
                provider.to_string(),
                provider_event_id.to_string(),
            ))
            .cloned()
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
    ) -> moa_knowledge::Result<KnowledgeConnection> {
        self.record_op("upsert_connection")?;
        self.with_state(|state| {
            state
                .connections
                .insert(connection.connection_uid, connection.clone());
            connection
        })
    }

    async fn get_connection(
        &self,
        connection_uid: Uuid,
    ) -> moa_knowledge::Result<Option<KnowledgeConnection>> {
        self.record_op("get_connection")?;
        self.with_state(|state| state.connections.get(&connection_uid).cloned())
    }

    async fn update_connection_source_selection(
        &self,
        connection_uid: Uuid,
        source_selection: Value,
    ) -> moa_knowledge::Result<KnowledgeConnection> {
        self.record_op("update_connection_source_selection")?;
        self.with_state(|state| {
            let connection = state.connections.get_mut(&connection_uid).ok_or_else(|| {
                KnowledgeError::Repository("connection should exist for fixture update".to_string())
            })?;
            connection.source_selection = source_selection;
            connection.last_synced_at = None;
            connection.updated_at = Utc::now();
            Ok(connection.clone())
        })?
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

    async fn lookup_connection_by_provider_account(
        &self,
        provider: &str,
        connector: Option<&str>,
        provider_account_id: &str,
    ) -> moa_knowledge::Result<ProviderAccountConnectionLookup> {
        self.record_op("lookup_connection_by_provider_account")?;
        self.with_state(|state| {
            let matches = state
                .connections
                .values()
                .filter(|connection| connection.provider == provider)
                .filter(|connection| {
                    connector.is_none_or(|connector| connector == connection.connector)
                })
                .filter(|connection| connection.provider_account_id == provider_account_id)
                .take(2)
                .cloned()
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [] => ProviderAccountConnectionLookup::NotFound,
                [connection] => ProviderAccountConnectionLookup::Unique(connection.clone()),
                matches => ProviderAccountConnectionLookup::Ambiguous {
                    matches: matches.len(),
                },
            }
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

    async fn latest_sync_run_for_connection(
        &self,
        connection_uid: Uuid,
        statuses: &[SyncRunStatus],
    ) -> moa_knowledge::Result<Option<KnowledgeSyncRun>> {
        self.record_op("latest_sync_run_for_connection")?;
        self.with_state(|state| {
            state
                .sync_runs
                .values()
                .filter(|run| run.connection_uid == connection_uid)
                .filter(|run| statuses.is_empty() || statuses.contains(&run.status))
                .max_by_key(|run| (run.started_at, run.sync_run_uid))
                .cloned()
        })
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
                run.records_changed += counters.records_changed;
                run.records_deleted += counters.records_deleted;
                run.records_ingested += counters.records_ingested;
                run.records_failed += counters.records_failed;
                run.objects_parsed += counters.objects_parsed;
                run.chunks_embedded += counters.chunks_embedded;
                run.graph_nodes_upserted += counters.graph_nodes_upserted;
                run.graph_edges_upserted += counters.graph_edges_upserted;
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

    async fn record_ingestion_step_once(
        &self,
        step: KnowledgeIngestionStep,
        counter_delta: KnowledgeSyncCounters,
    ) -> moa_knowledge::Result<bool> {
        self.record_op("record_ingestion_step_once")?;
        self.with_state(|state| {
            let step_object = step.object_uid.unwrap_or(Uuid::nil());
            let exists = state.steps.iter().any(|existing| {
                existing.sync_run_uid == step.sync_run_uid
                    && existing.object_uid.unwrap_or(Uuid::nil()) == step_object
                    && existing.step == step.step
                    && existing.retry_count == step.retry_count
            });
            if exists {
                return false;
            }
            if let Some(run) = state.sync_runs.get_mut(&step.sync_run_uid) {
                run.records_seen += counter_delta.records_seen;
                run.records_changed += counter_delta.records_changed;
                run.records_deleted += counter_delta.records_deleted;
                run.records_ingested += counter_delta.records_ingested;
                run.records_failed += counter_delta.records_failed;
                run.objects_parsed += counter_delta.objects_parsed;
                run.chunks_embedded += counter_delta.chunks_embedded;
                run.graph_nodes_upserted += counter_delta.graph_nodes_upserted;
                run.graph_edges_upserted += counter_delta.graph_edges_upserted;
            }
            state.steps.push(step);
            true
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

    async fn active_objects_for_connection(
        &self,
        connection_uid: Uuid,
    ) -> moa_knowledge::Result<Vec<KnowledgeObject>> {
        self.record_op("active_objects_for_connection")?;
        self.with_state(|state| {
            state
                .objects
                .values()
                .filter(|object| object.connection_uid == connection_uid)
                .filter(|object| object.status != ObjectStatus::Deleted)
                .cloned()
                .collect()
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

    async fn object_ingestion_completed_since(
        &self,
        object_uid: Uuid,
        since: DateTime<Utc>,
    ) -> moa_knowledge::Result<bool> {
        self.record_op("object_ingestion_completed_since")?;
        self.with_state(|state| {
            state.steps.iter().any(|step| {
                step.object_uid == Some(object_uid)
                    && step.step == "contact_groups_derived"
                    && step.status == moa_knowledge::domain::IngestionStepStatus::Completed
                    && step
                        .counters
                        .get("records_ingested")
                        .and_then(Value::as_u64)
                        == Some(1)
                    && step.ended_at.unwrap_or(step.started_at) >= since
            })
        })
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

    async fn contact_group_targets(
        &self,
        _tenant_id: TenantId,
        _group_key: &str,
    ) -> moa_knowledge::Result<Option<ContactGroupTarget>> {
        self.record_op("contact_group_targets")?;
        Ok(None)
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

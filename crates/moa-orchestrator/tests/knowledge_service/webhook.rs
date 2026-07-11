//! Webhook behavior for the tenant Knowledge service.

use super::*;

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
async fn provider_cdc_webhook_advances_provider_syncing_run_and_dispatches() {
    // Pins: provider CDC completion callbacks advance an existing provider-side run into local ingestion exactly once.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection =
        fixture_connection_for_provider(tenant_id, "nango", "google-drive", "provider-account-1");
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    repository
        .insert_connection(connection.clone())
        .expect("fixture Nango connection should be inserted");
    let sync_run_uid = Uuid::now_v7();
    repository
        .create_sync_run(KnowledgeSyncRun {
            sync_run_uid,
            tenant_id,
            connection_uid: connection.connection_uid,
            parser: Some("native".to_string()),
            max_records: Some(25),
            status: SyncRunStatus::ProviderSyncing,
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
        .expect("seed provider-syncing run");
    let service = fixture_webhook_service(repository.clone(), "nango", 80);
    let request = signed_connection_webhook_request(
        "nango",
        tenant_id,
        connection.connection_uid,
        "evt-nango-existing-run-completed",
        "sync.completed",
    );

    let response = service
        .provider_webhook(request)
        .await
        .expect("Nango completion should advance an existing provider run");
    let run = repository
        .get_sync_run(sync_run_uid)
        .await
        .expect("read seeded run")
        .expect("seeded run should still exist");
    let steps = repository
        .sync_run_steps(sync_run_uid, None)
        .await
        .expect("read provider CDC steps");

    assert_eq!(response.sync_run_uid, Some(sync_run_uid));
    assert!(response.ingestion_enqueued);
    assert_eq!(run.status, SyncRunStatus::ProviderSynced);
    assert_eq!(
        steps
            .iter()
            .map(|step| step.step.as_str())
            .collect::<Vec<_>>(),
        vec!["ingestion_enqueued"]
    );
    assert_eq!(repository.sync_run_count(), 1);
    assert_eq!(repository.provider_event_count(), 1);
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
    assert_eq!(repository.op_count("claim_sync_run"), 1);
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
async fn provider_webhook_rejects_unknown_provider_account_before_recording() {
    // Pins: a signed provider account identity with no local binding fails closed.
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    let service = KnowledgeService::new(
        repository.clone(),
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
            "event_id": "evt-unknown-account",
            "event_type": "sync.completed",
            "connection_id": "unknown-account",
            "provider_config_key": "google-drive"
        }),
    );

    let error = service
        .provider_webhook(request)
        .await
        .expect_err("unknown provider account should be rejected");

    assert!(error.to_string().contains("knowledge connection not found"));
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
    // Pins: parser webhook HMAC verification binds completion to an existing object/run and persists only safe event metadata.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let sync_run_uid = Uuid::now_v7();
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    let mut connection = fixture_connection(tenant_id);
    connection.connection_uid = connection_uid;
    repository
        .insert_connection(connection.clone())
        .expect("seed signed parser webhook connection");
    let object = fixture_object(tenant_id, connection_uid);
    repository
        .upsert_object(object.clone())
        .await
        .expect("seed parser webhook object");
    repository
        .create_sync_run(KnowledgeSyncRun {
            sync_run_uid,
            tenant_id,
            connection_uid,
            parser: Some("llamaparse".to_string()),
            max_records: Some(1),
            status: SyncRunStatus::ParsePending,
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
        .expect("seed parse-pending run");
    let verifier = Arc::new(
        ParserWebhookVerifier::new("llamaparse").with_signing_key("llamaparse-webhook-secret"),
    );
    let service = KnowledgeService::new(
        repository.clone(),
        repository.clone(),
        Arc::new(StaticKnowledgeProviders::new().with_webhook_verifier("llamaparse", verifier)),
        Arc::new(FakeKnowledgeCredentialStore::default()),
        fake_ingestion_runner(),
        80,
    );
    let payload = parser_webhook_payload(
        tenant_id,
        connection_uid,
        Some(object.object_uid),
        Some(&object.source_id),
        "lp-job-1",
    );
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
    let run = repository
        .get_sync_run(sync_run_uid)
        .await
        .expect("read parser webhook run")
        .expect("parser webhook run should exist");
    let steps = repository
        .sync_run_steps(sync_run_uid, None)
        .await
        .expect("read parser webhook steps");
    let stored_json =
        serde_json::to_string(&stored.payload).expect("stored payload should serialize");

    assert!(bad_error.to_string().contains("signature"));
    assert_eq!(response.provider, "llamaparse");
    assert_eq!(response.event_id, "lp-job-1");
    assert_eq!(response.sync_run_uid, Some(sync_run_uid));
    assert!(response.ingestion_enqueued);
    assert_eq!(run.status, SyncRunStatus::ProviderSynced);
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
    assert_eq!(
        steps
            .iter()
            .map(|step| (step.step.as_str(), step.object_uid))
            .collect::<Vec<_>>(),
        vec![
            ("ingestion_enqueued", None),
            ("parser_completion_received", Some(object.object_uid)),
        ]
    );
    assert_eq!(repository.provider_event_count(), 1);
    assert_eq!(repository.sync_run_count(), 1);
    assert_eq!(repository.step_count(), 2);
}

#[tokio::test]
async fn knowledge_auto_sync_parser_webhook_rejects_bad_custom_header_and_accepts_good_header() {
    // Pins: parser webhook custom-header verification binds completion by source id without provider API calls.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let sync_run_uid = Uuid::now_v7();
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    let mut connection = fixture_connection(tenant_id);
    connection.connection_uid = connection_uid;
    repository
        .insert_connection(connection.clone())
        .expect("seed signed parser webhook connection");
    let object = fixture_object(tenant_id, connection_uid);
    repository
        .upsert_object(object.clone())
        .await
        .expect("seed parser webhook object");
    repository
        .create_sync_run(KnowledgeSyncRun {
            sync_run_uid,
            tenant_id,
            connection_uid,
            parser: Some("reducto".to_string()),
            max_records: Some(1),
            status: SyncRunStatus::ParsePending,
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
        .expect("seed parse-pending run");
    let verifier = Arc::new(
        ParserWebhookVerifier::new("reducto")
            .with_custom_header("x-reducto-webhook-secret", "expected-header-secret"),
    );
    let service = KnowledgeService::new(
        repository.clone(),
        repository.clone(),
        Arc::new(StaticKnowledgeProviders::new().with_webhook_verifier("reducto", verifier)),
        Arc::new(FakeKnowledgeCredentialStore::default()),
        fake_ingestion_runner(),
        80,
    );
    let payload = parser_webhook_payload(
        tenant_id,
        connection_uid,
        None,
        Some(&object.source_id),
        "reducto-job-1",
    );
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
    let steps = repository
        .sync_run_steps(sync_run_uid, None)
        .await
        .expect("read parser custom-header webhook steps");

    assert!(bad_error.to_string().contains("header"));
    assert_eq!(response.provider, "reducto");
    assert_eq!(response.event_id, "reducto-job-1");
    assert_eq!(stored.connection_uid, Some(connection_uid));
    assert_eq!(response.sync_run_uid, Some(sync_run_uid));
    assert!(response.ingestion_enqueued);
    assert_eq!(
        steps
            .iter()
            .map(|step| (step.step.as_str(), step.object_uid))
            .collect::<Vec<_>>(),
        vec![
            ("ingestion_enqueued", None),
            ("parser_completion_received", Some(object.object_uid)),
        ]
    );
    assert_eq!(repository.provider_event_count(), 1);
    assert_eq!(repository.sync_run_count(), 1);
    assert_eq!(repository.step_count(), 2);
}

#[tokio::test]
async fn parser_completion_webhook_rejects_unbound_object_before_recording() {
    // Pins: a signed parser completion must identify a local object on the signed connection before any event row is stored.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    let mut connection = fixture_connection(tenant_id);
    connection.connection_uid = connection_uid;
    repository
        .insert_connection(connection)
        .expect("seed signed parser webhook connection");
    repository
        .create_sync_run(KnowledgeSyncRun {
            sync_run_uid: Uuid::now_v7(),
            tenant_id,
            connection_uid,
            parser: Some("llamaparse".to_string()),
            max_records: Some(1),
            status: SyncRunStatus::ParsePending,
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
        .expect("seed parse-pending run");
    let verifier = Arc::new(
        ParserWebhookVerifier::new("llamaparse").with_custom_header("x-parser-secret", "expected"),
    );
    let service = KnowledgeService::new(
        repository.clone(),
        repository.clone(),
        Arc::new(StaticKnowledgeProviders::new().with_webhook_verifier("llamaparse", verifier)),
        Arc::new(FakeKnowledgeCredentialStore::default()),
        fake_ingestion_runner(),
        80,
    );
    let payload = parser_webhook_payload(tenant_id, connection_uid, None, None, "lp-unbound");
    let request = parser_webhook_request(
        "llamaparse",
        payload,
        vec![("x-parser-secret".to_string(), "expected".to_string())],
    );

    let error = service
        .provider_webhook(request)
        .await
        .expect_err("unbound parser completion should be rejected");
    let error_text = error.to_string();

    assert!(error_text.contains("object_uid or source_id"));
    assert!(!error_text.contains(SECRET_TOKEN));
    assert!(!error_text.contains(RAW_DOCUMENT_TAIL));
    assert_eq!(repository.provider_event_count(), 0);
    assert_eq!(repository.step_count(), 0);
}

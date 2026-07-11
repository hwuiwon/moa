//! Inspection behavior for the tenant Knowledge service.

use super::*;

#[tokio::test]
async fn knowledge_service_accepts_injected_ingestion_runner_without_global_config() {
    // Pins: service tests can inject a deterministic ingestion runner without reading runtime config.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let sync_run_uid = Uuid::now_v7();
    let runner = Arc::new(FakeKnowledgeIngestionRunner::default());
    let service = KnowledgeService::new(
        Arc::new(InMemoryKnowledgeRepository::default()),
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

//! Idempotency ingestion scenarios.

use super::*;

#[tokio::test]
async fn ingestion_reconciles_stale_predecessor_when_retrying_incomplete_same_hash_version_db_memory()
 {
    // Pins: F07 — a version transition that persists new chunks but fails before
    // invalidating its predecessor leaves the newest version same-hash-incomplete.
    // The retry must reconcile against every active chunk across all versions and
    // orphan the real predecessor, instead of forgetting it (an empty
    // `previous_chunks`) and leaving BOTH versions' chunks active and retrievable.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope,
    ));
    let embedder = Arc::new(CountingEmbedder::default());
    let graph = Arc::new(FakeGraphWriter::default());
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        Arc::new(ParagraphParser),
        embedder.clone(),
        graph.clone(),
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 1,
                max_tokens: 16,
                min_tokens: 1,
            },
            provider: "test_provider".to_string(),
            parser_label: "test_parser".to_string(),
        },
    );
    repository
        .upsert_connection(KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: "test_provider".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "acct_1".to_string(),
            credential_ref: "vault://knowledge/test".to_string(),
            status: ConnectionStatus::Active,
            metadata: credentialish_metadata(),
            source_selection: json!({}),
            information_barrier: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_synced_at: None,
        })
        .await
        .expect("upsert connection");
    let object_uid = object_uid(connection_uid);

    // Attempt A: first content completes version V1 with two active chunks.
    let run_a = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            run_a,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("tok-a", false, "Alpha one.\n\nBeta one.")],
                next_cursor: None,
            },
        )
        .await
        .expect("ingest first content");
    assert_eq!(version_count(&pool, object_uid).await, 1);
    assert_eq!(active_chunk_count(&pool, object_uid).await, 2);

    // Attempt B: new content creates V2 and persists its chunks, then fails at
    // predecessor invalidation. Both V1 and V2 chunks are now active.
    graph.set_fail_invalidate(true);
    let run_b = create_run(&repository, tenant_id, connection_uid).await;
    let attempt_b = pipeline
        .ingest_record_page(
            run_b,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("tok-b", false, "Gamma two.\n\nDelta two.")],
                next_cursor: None,
            },
        )
        .await;
    assert!(
        attempt_b.is_err(),
        "failed predecessor invalidation must surface as an error"
    );
    assert_eq!(version_count(&pool, object_uid).await, 2);
    assert_eq!(
        active_chunk_count(&pool, object_uid).await,
        4,
        "both versions' chunks are stranded active before the retry"
    );

    // Attempt C: retry the same content (same hash, new change token). Invalidation
    // now succeeds; reconciliation must orphan V1's chunks rather than forget them.
    graph.set_fail_invalidate(false);
    let run_c = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            run_c,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("tok-c", false, "Gamma two.\n\nDelta two.")],
                next_cursor: None,
            },
        )
        .await
        .expect("retry same content");
    assert_eq!(
        version_count(&pool, object_uid).await,
        2,
        "the same-hash retry reuses V2 rather than creating a third version"
    );
    assert_eq!(
        active_chunk_count(&pool, object_uid).await,
        2,
        "only the newest version's chunks remain active after reconciliation"
    );
    assert_eq!(
        tombstoned_chunk_count(&pool, object_uid).await,
        2,
        "the stale predecessor's chunks are invalidated, not left active"
    );
}

#[tokio::test]
async fn ingestion_pipeline_replaying_same_page_keeps_counters_and_identities_once() {
    // Pins: replaying one provider page for the same sync run does not duplicate step counters or graph identities.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope,
    ));
    let parser = Arc::new(ParagraphParser);
    let embedder = Arc::new(CountingEmbedder::default());
    let graph = Arc::new(FakeGraphWriter::default());
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        parser,
        embedder.clone(),
        graph,
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 1,
                max_tokens: 16,
                min_tokens: 1,
            },
            provider: "test_provider".to_string(),
            parser_label: "test_parser".to_string(),
        },
    );

    repository
        .upsert_connection(KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: "test_provider".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "acct_replay".to_string(),
            credential_ref: "vault://knowledge/replay".to_string(),
            status: ConnectionStatus::Active,
            metadata: credentialish_metadata(),
            source_selection: json!({}),
            information_barrier: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_synced_at: None,
        })
        .await
        .expect("upsert connection");
    let sync_run_uid = create_run(&repository, tenant_id, connection_uid).await;

    let first = pipeline
        .ingest_record_page(
            sync_run_uid,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record(
                    "replay-token",
                    false,
                    "Alpha is ready.\n\nBudget is 10.",
                )],
                next_cursor: None,
            },
        )
        .await
        .expect("first page apply should ingest");
    assert_eq!(first.records_listed, 1);
    assert_eq!(first.records_ingested, 1);
    assert_eq!(first.records_skipped, 0);
    assert_eq!(first.embeddings_created, 2);

    let object_uid = object_uid(connection_uid);
    let first_counters = sync_counters(&pool, sync_run_uid).await;
    assert_eq!(first_counters.records_seen, 1);
    assert_eq!(first_counters.records_changed, 1);
    assert_eq!(first_counters.records_deleted, 0);
    assert_eq!(first_counters.records_ingested, 1);
    assert_eq!(first_counters.records_failed, 0);
    assert_eq!(first_counters.objects_parsed, 1);
    assert_eq!(first_counters.chunks_embedded, 2);
    assert!(first_counters.graph_nodes_upserted > 0);
    assert!(first_counters.graph_edges_upserted > 0);
    let first_identities = stored_identities(&pool, object_uid).await;
    assert_eq!(first_identities.object_uid, object_uid);

    let replay = pipeline
        .ingest_record_page(
            sync_run_uid,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record(
                    "replay-token",
                    false,
                    "Alpha is ready.\n\nBudget is 10.",
                )],
                next_cursor: None,
            },
        )
        .await
        .expect("replayed page apply should not fail on duplicate steps");
    assert_eq!(replay.records_listed, 1);
    assert_eq!(replay.records_ingested, 0);
    assert_eq!(replay.records_skipped, 1);
    assert_eq!(replay.embeddings_created, 0);

    assert_eq!(sync_counters(&pool, sync_run_uid).await, first_counters);
    assert_eq!(stored_identities(&pool, object_uid).await, first_identities);
    assert_eq!(embedder.embedded_count(), 2);

    let steps = repository
        .sync_run_steps(sync_run_uid, Some(object_uid))
        .await
        .expect("read object steps");
    assert_eq!(
        steps
            .iter()
            .filter(|step| step.step == "object_change_checked")
            .count(),
        1
    );
    let object_change = steps
        .iter()
        .find(|step| step.step == "object_change_checked")
        .expect("object change step should exist");
    assert_eq!(object_change.counters["records_seen"], json!(1));
    assert_eq!(object_change.counters["records_changed"], json!(1));
}

#[tokio::test]
async fn ingestion_pipeline_replay_after_change_token_only_progress_finishes_ingestion() {
    // Pins: replay after object/change-token advancement must resume missing parse, graph, vector, and final success work.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope,
    ));
    let parser = Arc::new(ParagraphParser);
    let embedder = Arc::new(CountingEmbedder::default());
    let graph = Arc::new(FakeGraphWriter::default());
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        parser,
        embedder.clone(),
        graph,
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 1,
                max_tokens: 16,
                min_tokens: 1,
            },
            provider: "test_provider".to_string(),
            parser_label: "test_parser".to_string(),
        },
    );

    repository
        .upsert_connection(KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: "test_provider".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "acct_partial_replay".to_string(),
            credential_ref: "vault://knowledge/partial-replay".to_string(),
            status: ConnectionStatus::Active,
            metadata: credentialish_metadata(),
            source_selection: json!({}),
            information_barrier: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_synced_at: None,
        })
        .await
        .expect("upsert connection");
    let sync_run_uid = create_run(&repository, tenant_id, connection_uid).await;
    let object_uid = object_uid(connection_uid);
    repository
        .upsert_object(KnowledgeObject {
            object_uid,
            tenant_id,
            connection_uid,
            object_type: "page".to_string(),
            source_id: "doc-1".to_string(),
            parent_source_id: None,
            source_uri: Some("https://example.test/doc-1".to_string()),
            title: Some("Alpha Plan".to_string()),
            change_token: Some("partial-token".to_string()),
            metadata: credentialish_metadata(),
            status: ObjectStatus::Active,
            source_updated_at: Some(Utc::now()),
            deleted_at: None,
        })
        .await
        .expect("seed partially advanced object row");
    repository
        .record_ingestion_step_once(
            KnowledgeIngestionStep {
                step_uid: Uuid::now_v7(),
                sync_run_uid,
                object_uid: Some(object_uid),
                step: "object_change_checked".to_string(),
                status: IngestionStepStatus::Completed,
                started_at: Utc::now(),
                ended_at: Some(Utc::now()),
                duration_ms: Some(1),
                counters: json!({ "records_seen": 1, "records_changed": 1 }),
                summary: None,
                retry_count: 0,
                error_code: None,
            },
            KnowledgeSyncCounters {
                records_seen: 1,
                records_changed: 1,
                ..KnowledgeSyncCounters::default()
            },
        )
        .await
        .expect("seed object change step")
        .then_some(())
        .expect("seeded object change step should insert");
    assert_eq!(
        sync_counters(&pool, sync_run_uid).await,
        Counters {
            records_seen: 1,
            records_changed: 1,
            records_deleted: 0,
            records_ingested: 0,
            records_failed: 0,
            objects_parsed: 0,
            chunks_embedded: 0,
            graph_nodes_upserted: 0,
            graph_edges_upserted: 0,
        }
    );

    let replay = pipeline
        .ingest_record_page(
            sync_run_uid,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record(
                    "partial-token",
                    false,
                    "Alpha is ready.\n\nBudget is 10.",
                )],
                next_cursor: None,
            },
        )
        .await
        .expect("partial-progress replay should finish ingestion");

    assert_eq!(replay.records_listed, 1);
    assert_eq!(replay.records_ingested, 1);
    assert_eq!(replay.records_skipped, 0);
    assert_eq!(replay.embeddings_created, 2);
    let counters = sync_counters(&pool, sync_run_uid).await;
    assert_eq!(counters.records_seen, 1);
    assert_eq!(counters.records_changed, 1);
    assert_eq!(counters.records_ingested, 1);
    assert_eq!(counters.records_failed, 0);
    assert_eq!(counters.objects_parsed, 1);
    assert_eq!(counters.chunks_embedded, 2);
    assert!(counters.graph_nodes_upserted > 0);
    assert!(counters.graph_edges_upserted > 0);
    assert_eq!(version_count(&pool, object_uid).await, 1);
    assert_eq!(chunk_count(&pool, object_uid).await, 2);
    assert_eq!(chunks_with_graph_uid(&pool, object_uid).await, 2);
    assert_eq!(embedder.embedded_count(), 2);
}

#[tokio::test]
async fn ingestion_pipeline_reclaims_stale_started_claim_after_crash_db_knowledge() {
    // Pins: retrying unchanged content after a crash reclaims the stale claim and finishes graph/vector writes.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope,
    ));
    let parser = Arc::new(ParagraphParser);
    let embedder = Arc::new(CountingEmbedder::default());
    let graph = Arc::new(FakeGraphWriter::default());
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        parser,
        embedder.clone(),
        graph.clone(),
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 1,
                max_tokens: 16,
                min_tokens: 1,
            },
            provider: "test_provider".to_string(),
            parser_label: "test_parser".to_string(),
        },
    );

    repository
        .upsert_connection(KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: "test_provider".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "acct_stale_claim_replay".to_string(),
            credential_ref: "vault://knowledge/stale-claim-replay".to_string(),
            status: ConnectionStatus::Active,
            metadata: credentialish_metadata(),
            source_selection: json!({}),
            information_barrier: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_synced_at: None,
        })
        .await
        .expect("upsert connection");
    let sync_run_uid = create_run(&repository, tenant_id, connection_uid).await;
    let object_uid = object_uid(connection_uid);
    let text = "Alpha is ready.\n\nBudget is 10.";
    repository
        .upsert_object(KnowledgeObject {
            object_uid,
            tenant_id,
            connection_uid,
            object_type: "page".to_string(),
            source_id: "doc-1".to_string(),
            parent_source_id: None,
            source_uri: Some("https://example.test/doc-1".to_string()),
            title: Some("Alpha Plan".to_string()),
            change_token: Some("stale-claim-token".to_string()),
            metadata: credentialish_metadata(),
            status: ObjectStatus::Active,
            source_updated_at: Some(Utc::now()),
            deleted_at: None,
        })
        .await
        .expect("seed object row advanced before crash");
    let normalized = normalize_text(text);
    let hash = content_hash(&normalized);
    let version = DocumentVersion {
        version_uid: moa_knowledge::graph_delta::stable_uid(&format!(
            "version:{object_uid}:{hash}"
        )),
        object_uid,
        parser: "test_parser".to_string(),
        parser_job_id: None,
        content_hash: hash,
        metadata: json!({ "crash": "after_claim" }),
        created_at: Utc::now(),
    };
    assert!(matches!(
        repository
            .claim_document_version_ingestion(sync_run_uid, version.clone())
            .await
            .expect("seed stale started claim"),
        moa_knowledge::repository::DocumentVersionIngestionClaim::Claimed { .. }
    ));
    expire_claim_lease(&pool, version.version_uid).await;

    let replay = pipeline
        .ingest_record_page(
            sync_run_uid,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("stale-claim-token", false, text)],
                next_cursor: None,
            },
        )
        .await
        .expect("stale-claim replay should finish ingestion");

    assert_eq!(replay.records_listed, 1);
    assert_eq!(replay.records_ingested, 1);
    assert_eq!(replay.records_skipped, 0);
    assert_eq!(replay.embeddings_created, 2);
    assert_eq!(version_count(&pool, object_uid).await, 1);
    assert_eq!(chunk_count(&pool, object_uid).await, 2);
    assert_eq!(chunks_with_graph_uid(&pool, object_uid).await, 2);
    assert_eq!(embedder.embedded_count(), 2);
    assert_eq!(graph.vector_count(), 2);
    assert_eq!(
        document_version_claim_status(&pool, version.version_uid).await,
        "completed"
    );
}

#[tokio::test]
async fn ingestion_pipeline_replay_after_graph_uid_midpoint_finishes_ingestion() {
    // Pins: graph_node_uid on chunks is not enough replay proof before final records_ingested step.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope,
    ));
    let parser = Arc::new(ParagraphParser);
    let embedder = Arc::new(CountingEmbedder::default());
    let graph = Arc::new(FakeGraphWriter::default());
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        parser,
        embedder.clone(),
        graph,
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 1,
                max_tokens: 16,
                min_tokens: 1,
            },
            provider: "test_provider".to_string(),
            parser_label: "test_parser".to_string(),
        },
    );

    repository
        .upsert_connection(KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: "test_provider".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "acct_graph_uid_replay".to_string(),
            credential_ref: "vault://knowledge/graph-uid-replay".to_string(),
            status: ConnectionStatus::Active,
            metadata: credentialish_metadata(),
            source_selection: json!({}),
            information_barrier: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_synced_at: None,
        })
        .await
        .expect("upsert connection");
    let sync_run_uid = create_run(&repository, tenant_id, connection_uid).await;
    let object_uid = object_uid(connection_uid);
    repository
        .upsert_object(KnowledgeObject {
            object_uid,
            tenant_id,
            connection_uid,
            object_type: "page".to_string(),
            source_id: "doc-1".to_string(),
            parent_source_id: None,
            source_uri: Some("https://example.test/doc-1".to_string()),
            title: Some("Alpha Plan".to_string()),
            change_token: Some("graph-midpoint-token".to_string()),
            metadata: credentialish_metadata(),
            status: ObjectStatus::Active,
            source_updated_at: Some(Utc::now()),
            deleted_at: None,
        })
        .await
        .expect("seed partially advanced object row");
    repository
        .record_ingestion_step_once(
            KnowledgeIngestionStep {
                step_uid: Uuid::now_v7(),
                sync_run_uid,
                object_uid: Some(object_uid),
                step: "object_change_checked".to_string(),
                status: IngestionStepStatus::Completed,
                started_at: Utc::now(),
                ended_at: Some(Utc::now()),
                duration_ms: Some(1),
                counters: json!({ "records_seen": 1, "records_changed": 1 }),
                summary: None,
                retry_count: 0,
                error_code: None,
            },
            KnowledgeSyncCounters {
                records_seen: 1,
                records_changed: 1,
                ..KnowledgeSyncCounters::default()
            },
        )
        .await
        .expect("seed object change step")
        .then_some(())
        .expect("seeded object change step should insert");
    seed_graph_linked_partial_version(&repository, object_uid, "Alpha is ready.\n\nBudget is 10.")
        .await;

    let replay = pipeline
        .ingest_record_page(
            sync_run_uid,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record(
                    "graph-midpoint-token",
                    false,
                    "Alpha is ready.\n\nBudget is 10.",
                )],
                next_cursor: None,
            },
        )
        .await
        .expect("graph-uid midpoint replay should finish ingestion");

    assert_eq!(replay.records_listed, 1);
    assert_eq!(replay.records_ingested, 1);
    assert_eq!(replay.records_skipped, 0);
    assert_eq!(replay.embeddings_created, 2);
    let counters = sync_counters(&pool, sync_run_uid).await;
    assert_eq!(counters.records_seen, 1);
    assert_eq!(counters.records_changed, 1);
    assert_eq!(counters.records_ingested, 1);
    assert_eq!(counters.records_failed, 0);
    assert_eq!(counters.objects_parsed, 1);
    assert_eq!(counters.chunks_embedded, 2);
    assert!(counters.graph_nodes_upserted > 0);
    assert!(counters.graph_edges_upserted > 0);
    assert_eq!(chunks_with_graph_uid(&pool, object_uid).await, 2);
    assert_eq!(embedder.embedded_count(), 2);
}

#[tokio::test]
async fn ingestion_pipeline_reingests_inline_edit_under_unchanged_token_db_memory() {
    // Pins: the version-hash guard still forces re-ingestion when an inline-text
    // record's content changes under an unchanged change token — the case the
    // hash comparison protects.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope,
    ));
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        Arc::new(ParagraphParser),
        Arc::new(CountingEmbedder::default()),
        Arc::new(FakeGraphWriter::default()),
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 1,
                max_tokens: 16,
                min_tokens: 1,
            },
            provider: "test_provider".to_string(),
            parser_label: "test_parser".to_string(),
        },
    );

    repository
        .upsert_connection(drive_connection(connection_uid, tenant_id))
        .await
        .expect("upsert connection");
    let object_uid = object_uid(connection_uid);

    let first_run = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            first_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("same-token", false, "Alpha one.")],
                next_cursor: None,
            },
        )
        .await
        .expect("first inline sync should ingest");
    assert_eq!(version_count(&pool, object_uid).await, 1);

    // Same change token, edited inline text: the hash guard forces re-ingestion.
    let edit_run = create_run(&repository, tenant_id, connection_uid).await;
    let edited = pipeline
        .ingest_record_page(
            edit_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("same-token", false, "Beta two.")],
                next_cursor: None,
            },
        )
        .await
        .expect("inline edit under unchanged token should re-ingest");
    assert_eq!(edited.records_ingested, 1);
    assert_eq!(edited.records_skipped, 0);
    assert_eq!(version_count(&pool, object_uid).await, 2);
}

//! Deletion ingestion scenarios.

use super::*;

#[tokio::test]
async fn ingestion_pipeline_skips_unchanged_reembeds_edits_and_tombstones_deletes() {
    // Pins: provider-page ingestion is idempotent, re-embeds only changed chunks, and keeps DB audit rows.
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
            provider_account_id: "acct_1".to_string(),
            credential_ref: "vault://knowledge/test".to_string(),
            status: ConnectionStatus::Active,
            metadata: credentialish_metadata(),
            source_selection: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_synced_at: None,
        })
        .await
        .expect("upsert connection");
    let connection_metadata = connection_metadata(&pool, connection_uid).await;
    assert_eq!(connection_metadata["safe"], true);
    assert_no_secret_material(&connection_metadata);

    let first_run = create_run(&repository, tenant_id, connection_uid).await;
    let first = pipeline
        .ingest_record_page(
            first_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("v1", false, "Alpha is ready.\n\nBudget is 10.")],
                next_cursor: None,
            },
        )
        .await
        .expect("ingest first record");
    assert_eq!(first.records_ingested, 1);
    assert_eq!(first.embeddings_created, 2);
    let first_counters = sync_counters(&pool, first_run).await;
    assert_eq!(first_counters.records_seen, 1);
    assert_eq!(first_counters.records_changed, 1);
    assert_eq!(first_counters.records_ingested, 1);
    assert_eq!(embedder.embedded_count(), 2);
    assert_eq!(graph.vector_count(), 2);
    let object_uid = object_uid(connection_uid);
    let first_object_metadata = object_metadata(&pool, object_uid).await;
    assert_eq!(first_object_metadata["mime_type"], "text/plain");
    assert_eq!(first_object_metadata["safe"], true);
    assert_eq!(
        first_object_metadata["nested"]["session_header"],
        "[redacted]"
    );
    assert_no_secret_material(&first_object_metadata);
    assert_eq!(version_count(&pool, object_uid).await, 1);
    assert_eq!(chunk_count(&pool, object_uid).await, 2);
    assert_eq!(chunks_with_graph_uid(&pool, object_uid).await, 2);

    let unchanged_run = create_run(&repository, tenant_id, connection_uid).await;
    let unchanged = pipeline
        .ingest_record_page(
            unchanged_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("v1", false, "Alpha is ready.\n\nBudget is 10.")],
                next_cursor: None,
            },
        )
        .await
        .expect("skip unchanged record");
    assert_eq!(unchanged.records_skipped, 1);
    let unchanged_counters = sync_counters(&pool, unchanged_run).await;
    assert_eq!(unchanged_counters.records_seen, 1);
    assert_eq!(unchanged_counters.records_changed, 0);
    assert_eq!(unchanged_counters.records_ingested, 0);
    assert_eq!(embedder.embedded_count(), 2);
    assert_eq!(version_count(&pool, object_uid).await, 1);
    assert_eq!(chunk_count(&pool, object_uid).await, 2);

    let edit_run = create_run(&repository, tenant_id, connection_uid).await;
    let edited = pipeline
        .ingest_record_page(
            edit_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("v2", false, "Alpha is ready.\n\nBudget is 20.")],
                next_cursor: None,
            },
        )
        .await
        .expect("ingest one-block edit");
    assert_eq!(edited.records_ingested, 1);
    assert_eq!(edited.embeddings_created, 1);
    assert_eq!(embedder.embedded_count(), 3);
    assert_eq!(graph.vector_count(), 2);
    assert_eq!(graph.invalidated_count(), 1);
    assert_eq!(version_count(&pool, object_uid).await, 2);
    assert_eq!(chunk_count(&pool, object_uid).await, 4);
    assert_eq!(tombstoned_chunk_count(&pool, object_uid).await, 1);

    let delete_run = create_run(&repository, tenant_id, connection_uid).await;
    let deleted = pipeline
        .ingest_record_page(
            delete_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("v3", true, "")],
                next_cursor: None,
            },
        )
        .await
        .expect("handle provider deletion");
    assert_eq!(deleted.records_deleted, 1);
    let delete_counters = sync_counters(&pool, delete_run).await;
    assert_eq!(delete_counters.records_seen, 1);
    assert_eq!(delete_counters.records_deleted, 1);
    assert_eq!(graph.vector_count(), 0);
    assert_eq!(object_status(&pool, object_uid).await, "deleted");
    assert_eq!(tombstoned_chunk_count(&pool, object_uid).await, 3);

    let steps = repository
        .object_timeline(object_uid)
        .await
        .expect("read object ingestion timeline");
    let first_run_steps = steps
        .iter()
        .filter(|step| step.sync_run_uid == first_run)
        .map(|step| step.step.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        first_run_steps,
        vec![
            "object_change_checked",
            "content_fetched",
            "parse_submitted",
            "parse_completed",
            "normalized",
            "blocks_diffed",
            "semantic_graph_extracted",
            "chunks_diffed",
            "embedded",
            "graph_upserted",
            "vector_indexed",
            "contact_groups_derived",
        ]
    );
    let unchanged_steps = steps
        .iter()
        .filter(|step| step.sync_run_uid == unchanged_run)
        .collect::<Vec<_>>();
    assert_eq!(unchanged_steps.len(), 1);
    assert_eq!(
        unchanged_steps[0].status,
        moa_knowledge::domain::IngestionStepStatus::Skipped
    );

    let counters = sync_counters(&pool, edit_run).await;
    assert_eq!(counters.records_seen, 1);
    assert_eq!(counters.records_changed, 1);
    assert_eq!(counters.records_ingested, 1);
    assert_eq!(counters.chunks_embedded, 1);

    let graph_json = graph.properties_json();
    assert_no_secret_text(&graph_json);
}

#[tokio::test]
async fn deletion_writes_terminal_status_last_and_stays_retryable_on_invalidation_failure_db_memory()
 {
    // Pins: F06 — a provider deletion whose graph invalidation fails must NOT leave
    // the object in terminal `deleted` with active chunks stranded. The object stays
    // non-terminal (so it is still selectable), and a later prune completes the
    // deletion. This covers both the `handle_deleted_record` entry point (no direct
    // upsert into terminal `deleted`) and the terminal-state-last ordering.
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
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_synced_at: None,
        })
        .await
        .expect("upsert connection");
    let object_uid = object_uid(connection_uid);

    // Ingest content so the object has one active version with two active chunks.
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
        .expect("ingest content");
    assert_eq!(active_chunk_count(&pool, object_uid).await, 2);
    assert_eq!(object_status(&pool, object_uid).await, "active");

    // Provider deletion with failing invalidation: the object must remain
    // non-terminal (never upserted straight into `deleted`) and keep its chunks.
    graph.set_fail_invalidate(true);
    let run_del = create_run(&repository, tenant_id, connection_uid).await;
    let failed = pipeline
        .ingest_record_page(
            run_del,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("tok-del", true, "")],
                next_cursor: None,
            },
        )
        .await;
    assert!(
        failed.is_err(),
        "failed deletion invalidation must surface as an error"
    );
    assert_eq!(
        object_status(&pool, object_uid).await,
        "active",
        "terminal `deleted` must not be written before cleanup succeeds"
    );
    assert_eq!(
        active_chunk_count(&pool, object_uid).await,
        2,
        "chunks stay active and selectable for a cleanup retry"
    );

    // Retry via prune: the still-active object is selected and deletion completes.
    graph.set_fail_invalidate(false);
    let run_prune = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .prune_unseen_objects(
            run_prune,
            connection_uid,
            tenant_id,
            &std::collections::HashSet::new(),
        )
        .await
        .expect("prune completes the stranded deletion");
    assert_eq!(
        object_status(&pool, object_uid).await,
        "deleted",
        "deletion completes once invalidation succeeds"
    );
    assert_eq!(
        active_chunk_count(&pool, object_uid).await,
        0,
        "all chunks are invalidated once the deletion completes"
    );
    assert_eq!(tombstoned_chunk_count(&pool, object_uid).await, 2);
}

#[tokio::test]
async fn ingestion_pipeline_prunes_unseen_objects_after_full_selection_refresh() {
    // Pins: full selected-source refreshes tombstone previously active objects absent from the provider listing.
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
        embedder,
        graph.clone(),
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 4,
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
            provider_account_id: "acct_prune".to_string(),
            credential_ref: "vault://knowledge/prune".to_string(),
            status: ConnectionStatus::Active,
            metadata: json!({}),
            source_selection: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_synced_at: None,
        })
        .await
        .expect("upsert connection");
    let initial_run = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            initial_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![
                    record_with_source("doc-keep", "v1", false, "Keep this source."),
                    record_with_source("doc-drop", "v1", false, "Drop this source."),
                ],
                next_cursor: None,
            },
        )
        .await
        .expect("ingest initial selected sources");
    assert_eq!(graph.vector_count(), 2);

    let prune_run = create_run(&repository, tenant_id, connection_uid).await;
    let seen_source_ids = HashSet::from(["doc-keep".to_string()]);
    let report = pipeline
        .prune_unseen_objects(prune_run, connection_uid, tenant_id, &seen_source_ids)
        .await
        .expect("prune sources absent from full refresh");

    let keep_uid = object_uid_for_source(connection_uid, "doc-keep");
    let drop_uid = object_uid_for_source(connection_uid, "doc-drop");
    assert_eq!(report.records_deleted, 1);
    assert_eq!(object_status(&pool, keep_uid).await, "active");
    assert_eq!(object_status(&pool, drop_uid).await, "deleted");
    assert_eq!(graph.vector_count(), 1);
    assert_eq!(graph.invalidated_count(), 1);
    assert_eq!(tombstoned_chunk_count(&pool, drop_uid).await, 1);
    assert_eq!(tombstoned_chunk_count(&pool, keep_uid).await, 0);

    let counters = sync_counters(&pool, prune_run).await;
    assert_eq!(counters.records_seen, 0);
    assert_eq!(counters.records_deleted, 1);
    let run_steps = repository
        .sync_run_steps(prune_run, None)
        .await
        .expect("read prune run timeline");
    let rendered_steps = run_steps
        .iter()
        .map(|step| format!("{} {}", step.step, step.counters))
        .collect::<Vec<_>>()
        .join(", ");
    assert!(
        run_steps
            .iter()
            .any(|step| step.step == "source_selection_pruned"
                && step.counters["records_pruned"] == json!(1)),
        "prune run should record a run-level selected-source prune step; got {rendered_steps}"
    );
}

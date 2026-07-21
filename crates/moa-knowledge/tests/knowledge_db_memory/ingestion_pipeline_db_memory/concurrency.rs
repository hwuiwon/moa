//! Concurrency ingestion scenarios.

use super::*;

#[tokio::test]
async fn embedding_cardinality_mismatch_rejects_batch_without_graph_write_db_memory() {
    // Pins: F08 — a provider that returns fewer vectors than inputs fails the
    // version ingestion with a typed cardinality error and writes nothing to the
    // graph, instead of silently zipping and dropping/misaligning chunks.
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
    let graph = Arc::new(FakeGraphWriter::default());
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        Arc::new(ParagraphParser),
        Arc::new(MiscountingEmbedder),
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

    let run = create_run(&repository, tenant_id, connection_uid).await;
    // Two paragraphs -> two chunks -> two embedding inputs; the embedder returns one.
    let result = pipeline
        .ingest_record_page(
            run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("tok-1", false, "Alpha one.\n\nBeta one.")],
                next_cursor: None,
            },
        )
        .await;

    assert!(
        matches!(
            result,
            Err(moa_knowledge::Error::EmbeddingCardinalityMismatch { .. })
        ),
        "expected a typed cardinality error, got {result:?}"
    );
    assert_eq!(
        graph.vector_count(),
        0,
        "no vectors are written when the embedding batch is rejected"
    );
}

#[tokio::test]
async fn ingest_record_page_processes_records_concurrently_with_accurate_report_db_memory() {
    // Pins: F26 — a page of distinct records all ingest under bounded concurrency
    // and the folded report is accurate.
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

    let record_count = 6_u64;
    let records = (0..record_count)
        .map(|index| {
            record_with_source(
                &format!("doc-{index}"),
                "tok-1",
                false,
                &format!("Alpha {index}.\n\nBeta {index}."),
            )
        })
        .collect::<Vec<_>>();
    let run = create_run(&repository, tenant_id, connection_uid).await;

    let report = pipeline
        .ingest_record_page(
            run,
            connection_uid,
            tenant_id,
            RecordPage {
                records,
                next_cursor: None,
            },
        )
        .await
        .expect("page of distinct records ingests concurrently");

    assert_eq!(report.records_listed, record_count);
    assert_eq!(report.records_ingested, record_count);
    assert_eq!(report.records_skipped, 0);
    assert_eq!(report.records_deleted, 0);
    // Every distinct object was persisted with an active version.
    for index in 0..record_count {
        let object_uid = object_uid_for_source(connection_uid, &format!("doc-{index}"));
        assert_eq!(
            version_count(&pool, object_uid).await,
            1,
            "each concurrently ingested record has one document version"
        );
    }
}

#[tokio::test]
async fn ingestion_pipeline_duplicate_workers_coalesce_object_version_before_graph_writes_db_knowledge()
 {
    // Pins: duplicate object delivery from two workers stores one document version and performs one graph/vector write.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let scope = RlsContext::tenant(tenant_id);
    let repository_a = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope.clone(),
    ));
    let repository_b = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope,
    ));
    let parser = Arc::new(BarrierParser {
        barrier: Arc::new(Barrier::new(2)),
    });
    let embedder = Arc::new(CountingEmbedder::default());
    let graph = Arc::new(FakeGraphWriter::default());
    let pipeline_a = KnowledgeIngestionPipeline::new(
        repository_a.clone(),
        parser.clone(),
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
    let pipeline_b = KnowledgeIngestionPipeline::new(
        repository_b.clone(),
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

    repository_a
        .upsert_connection(KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: "test_provider".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "acct_duplicate_workers".to_string(),
            credential_ref: "vault://knowledge/duplicate-workers".to_string(),
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
    let sync_run_uid = create_run(&repository_a, tenant_id, connection_uid).await;
    let page = RecordPage {
        records: vec![record(
            "duplicate-token",
            false,
            "Alpha is ready.\n\nBudget is 10.",
        )],
        next_cursor: None,
    };

    let (result_a, result_b) = tokio::join!(
        pipeline_a.ingest_record_page(sync_run_uid, connection_uid, tenant_id, page.clone()),
        pipeline_b.ingest_record_page(sync_run_uid, connection_uid, tenant_id, page)
    );
    let result_a = result_a.expect("first worker should finish");
    let result_b = result_b.expect("second worker should finish");
    assert_eq!(result_a.records_ingested + result_b.records_ingested, 1);
    assert_eq!(result_a.records_skipped + result_b.records_skipped, 1);

    let object_uid = object_uid(connection_uid);
    assert_eq!(version_count(&pool, object_uid).await, 1);
    assert_eq!(chunk_count(&pool, object_uid).await, 2);
    assert_eq!(embedder.embedded_count(), 2);
    assert_eq!(graph.vector_count(), 2);
    assert_eq!(
        completed_ingestion_step_count(&pool, sync_run_uid, object_uid).await,
        1
    );
    let counters = sync_counters(&pool, sync_run_uid).await;
    assert_eq!(counters.records_ingested, 1);
    assert_eq!(counters.chunks_embedded, 2);
}

//! Content fetch ingestion scenarios.

use super::*;

#[tokio::test]
async fn ingestion_pipeline_fetches_content_for_metadata_only_records_db_memory() {
    // Pins: a metadata-only provider record (no inline text, no fetchable URL)
    // has its content downloaded through the content fetcher, and the fetched
    // bytes are chunked and stored as real content instead of a title stub.
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
    let fetched = b"Fetched alpha.\n\nFetched beta.".to_vec();
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        Arc::new(BytesOrTextParagraphParser),
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
    )
    .with_content_fetcher(Some(Arc::new(FakeContentFetcher::new(
        FetchOutcome::Bytes(fetched.clone(), Some("text/plain".to_string())),
    ))));

    repository
        .upsert_connection(drive_connection(connection_uid, tenant_id))
        .await
        .expect("upsert connection");
    let run = create_run(&repository, tenant_id, connection_uid).await;
    let report = pipeline
        .ingest_record_page(
            run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![metadata_only_record("doc-1", "v1", "Roadmap")],
                next_cursor: None,
            },
        )
        .await
        .expect("metadata-only record should ingest via fetched content");
    assert_eq!(report.records_ingested, 1);

    let object_uid = object_uid(connection_uid);
    let version = repository
        .latest_document_version(object_uid)
        .await
        .expect("load version")
        .expect("fetched content should create a version");
    let chunks = repository
        .chunks_for_version(version.version_uid)
        .await
        .expect("load chunks");
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk.text.clone())
            .collect::<Vec<_>>(),
        vec!["Fetched alpha.".to_string(), "Fetched beta.".to_string()],
        "chunks should carry fetched content, not the title"
    );

    let steps = repository
        .sync_run_steps(run, Some(object_uid))
        .await
        .expect("read object steps");
    let content_step = steps
        .iter()
        .find(|step| step.step == "content_fetched")
        .expect("content_fetched step should be recorded");
    assert_eq!(content_step.status, IngestionStepStatus::Completed);
    assert_eq!(content_step.error_code, None);
    assert_eq!(
        content_step.counters["bytes_fetched"],
        json!(fetched.len()),
        "content_fetched should report the fetched byte count"
    );
}

#[tokio::test]
async fn ingestion_pipeline_falls_back_to_title_when_content_fetch_fails_db_memory() {
    // Pins: a failed content fetch keeps the title-only fallback (record still
    // ingests, run not failed) but records a distinct content_fetch failure code
    // so it is not confused with a plain metadata-only record.
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
        Arc::new(BytesOrTextParagraphParser),
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
    )
    .with_content_fetcher(Some(Arc::new(FakeContentFetcher::new(FetchOutcome::Error))));

    repository
        .upsert_connection(drive_connection(connection_uid, tenant_id))
        .await
        .expect("upsert connection");
    let run = create_run(&repository, tenant_id, connection_uid).await;
    let report = pipeline
        .ingest_record_page(
            run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![metadata_only_record("doc-1", "v1", "Fallback Title")],
                next_cursor: None,
            },
        )
        .await
        .expect("failed fetch should not fail the page");
    assert_eq!(report.records_ingested, 1);

    let object_uid = object_uid(connection_uid);
    let version = repository
        .latest_document_version(object_uid)
        .await
        .expect("load version")
        .expect("title fallback should create a version");
    let chunks = repository
        .chunks_for_version(version.version_uid)
        .await
        .expect("load chunks");
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk.text.clone())
            .collect::<Vec<_>>(),
        vec!["Fallback Title".to_string()],
        "failed fetch should fall back to indexing the record title"
    );

    let steps = repository
        .sync_run_steps(run, Some(object_uid))
        .await
        .expect("read object steps");
    let content_step = steps
        .iter()
        .find(|step| step.step == "content_fetched")
        .expect("content_fetched step should be recorded");
    assert_eq!(content_step.status, IngestionStepStatus::Completed);
    assert_eq!(
        content_step.error_code.as_deref(),
        Some("provider_content_fetch_failed"),
        "failed fetch must be distinguishable from a plain metadata-only record"
    );

    let run_row = repository
        .get_sync_run(run)
        .await
        .expect("read run")
        .expect("run should exist");
    assert_eq!(run_row.records_failed, 0);
    assert!(
        !matches!(
            run_row.status,
            SyncRunStatus::FailedRetryable | SyncRunStatus::FailedTerminal
        ),
        "content fetch failure must not fail the run: {:?}",
        run_row.status
    );
}

#[tokio::test]
async fn ingestion_pipeline_skips_unchanged_fetched_content_without_refetching_db_memory() {
    // Pins: a metadata-only record whose content came from the fetch hook is not
    // re-fetched on an unchanged-change-token sync (no second fetch, no new
    // version), while a changed change token does trigger a re-fetch.
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
    let fetcher = Arc::new(FakeContentFetcher::new(FetchOutcome::Bytes(
        b"Fetched alpha.\n\nFetched beta.".to_vec(),
        Some("text/plain".to_string()),
    )));
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        Arc::new(BytesOrTextParagraphParser),
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
    )
    .with_content_fetcher(Some(fetcher.clone()));

    repository
        .upsert_connection(drive_connection(connection_uid, tenant_id))
        .await
        .expect("upsert connection");
    let object_uid = object_uid(connection_uid);

    // First sync (token v1): fetches and ingests.
    let first_run = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            first_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![metadata_only_record("doc-1", "v1", "Roadmap")],
                next_cursor: None,
            },
        )
        .await
        .expect("first sync should ingest fetched content");
    assert_eq!(fetcher.calls(), 1);
    assert_eq!(version_count(&pool, object_uid).await, 1);

    // Re-sync with the same change token: must skip without re-fetching.
    let unchanged_run = create_run(&repository, tenant_id, connection_uid).await;
    let unchanged = pipeline
        .ingest_record_page(
            unchanged_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![metadata_only_record("doc-1", "v1", "Roadmap")],
                next_cursor: None,
            },
        )
        .await
        .expect("unchanged re-sync should succeed");
    assert_eq!(unchanged.records_skipped, 1);
    assert_eq!(unchanged.records_ingested, 0);
    assert_eq!(
        fetcher.calls(),
        1,
        "unchanged change token must not re-fetch content"
    );
    assert_eq!(version_count(&pool, object_uid).await, 1);

    // A changed change token re-fetches (the provider signaled a change).
    let changed_run = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            changed_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![metadata_only_record("doc-1", "v2", "Roadmap")],
                next_cursor: None,
            },
        )
        .await
        .expect("changed-token re-sync should succeed");
    assert_eq!(
        fetcher.calls(),
        2,
        "a changed change token must re-fetch content"
    );
}

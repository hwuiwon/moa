use super::*;

#[tokio::test]
async fn ingestion_preserves_chunk_structure_for_bounded_neighbor_context_db_memory() {
    // Pins: ingested chunks preserve document version, ordinal, heading path, and active status for bounded neighbor lookup.
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

    insert_managed_connector_parent(
        &pool,
        tenant_id,
        connection_uid,
        moa_knowledge::domain::LinkedProviderKind::Nango,
    )
    .await;
    repository
        .upsert_connection(KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: moa_knowledge::domain::LinkedProviderKind::Nango,
            connector: "docs".to_string(),
            provider_account_id: "acct_structure_audit".to_string(),
            metadata: json!({}),
            source_selection: json!({}),
            information_barrier: None,
            created_at: moa_test_support::fixtures::pg_now(),
            updated_at: moa_test_support::fixtures::pg_now(),
            last_synced_at: None,
        })
        .await
        .expect("upsert connection");
    let sync_run_uid = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            sync_run_uid,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record(
                    "structure-token",
                    false,
                    "Eligibility alpha.\n\nApproval bravo.\n\nCarryover charlie.",
                )],
                next_cursor: None,
            },
        )
        .await
        .expect("ingest structure audit record");

    let object_uid = object_uid(connection_uid);
    let version = repository
        .latest_document_version(object_uid)
        .await
        .expect("load latest version")
        .expect("ingestion should create a document version");
    let chunks = repository
        .chunks_for_version(version.version_uid)
        .await
        .expect("load chunks for version");
    assert_eq!(chunks.len(), 3);
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk.version_uid)
            .collect::<Vec<_>>(),
        vec![
            version.version_uid,
            version.version_uid,
            version.version_uid
        ]
    );
    assert_eq!(
        chunks.iter().map(|chunk| chunk.ordinal).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(
        chunks_with_occurrence_identity(&pool, object_uid).await,
        3,
        "every chunk's persisted graph identity is its own occurrence identity"
    );
    assert!(
        chunks
            .iter()
            .all(|chunk| chunk.metadata["active"] == json!(true)),
        "{chunks:?}"
    );
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk.heading_path.clone())
            .collect::<Vec<_>>(),
        vec![
            vec!["Alpha Plan".to_string()],
            vec!["Alpha Plan".to_string()],
            vec!["Alpha Plan".to_string()],
        ]
    );

    let adjacent = active_adjacent_chunk_rows(&pool, chunks[1].chunk_uid).await;
    assert_eq!(
        adjacent.iter().map(|row| row.ordinal).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert!(
        adjacent
            .iter()
            .all(|row| row.version_uid == version.version_uid),
        "{adjacent:?}"
    );
    assert!(
        adjacent
            .iter()
            .all(|row| row.heading_path == vec!["Alpha Plan".to_string()]),
        "{adjacent:?}"
    );
    assert!(
        adjacent.iter().all(|row| row.active == "true"),
        "{adjacent:?}"
    );
    assert_eq!(
        adjacent
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>(),
        vec![
            "Eligibility alpha.",
            "Approval bravo.",
            "Carryover charlie."
        ]
    );

    repository
        .tombstone_chunks(&[chunks[0].chunk_uid])
        .await
        .expect("tombstone previous chunk");
    let active_after_tombstone = active_adjacent_chunk_rows(&pool, chunks[1].chunk_uid).await;
    assert_eq!(
        active_after_tombstone
            .iter()
            .map(|row| row.ordinal)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(
        active_after_tombstone
            .iter()
            .all(|row| row.active == "true"),
        "{active_after_tombstone:?}"
    );
}

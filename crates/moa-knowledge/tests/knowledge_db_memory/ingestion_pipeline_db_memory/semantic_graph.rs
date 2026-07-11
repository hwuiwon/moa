//! Semantic graph ingestion scenarios.

use super::*;

#[tokio::test]
async fn semantic_graph_extraction_is_cached_reported_and_written_db_memory() {
    // Pins: semantic graph extraction is a persisted ingestion-time cache with
    // reported hit/miss counters and graph-visible typed edges.
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
            provider_account_id: "acct_semantic".to_string(),
            credential_ref: "vault://knowledge/semantic".to_string(),
            status: ConnectionStatus::Active,
            metadata: json!({}),
            source_selection: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_synced_at: None,
        })
        .await
        .expect("upsert connection");
    let text = "Connecting a custom domain requires a premium plan and DNS records.\n\nTroubleshoot domain not working by checking DNS.";
    let sync_run_uid = create_run(&repository, tenant_id, connection_uid).await;

    let result = pipeline
        .ingest_record_page(
            sync_run_uid,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![
                    record_with_source("semantic-a", "v1", false, text),
                    record_with_source("semantic-b", "v1", false, text),
                ],
                next_cursor: None,
            },
        )
        .await
        .expect("ingest semantic records");

    assert_eq!(result.records_ingested, 2);
    let first_object_uid = object_uid_for_source(connection_uid, "semantic-a");
    let second_object_uid = object_uid_for_source(connection_uid, "semantic-b");
    let first_counters = semantic_graph_step_counters(&pool, sync_run_uid, first_object_uid).await;
    let second_counters =
        semantic_graph_step_counters(&pool, sync_run_uid, second_object_uid).await;
    assert_eq!(first_counters["chunks_total"], 2);
    assert_eq!(second_counters["chunks_total"], 2);
    // With bounded page concurrency the two same-content records may run in
    // parallel, so deterministic intra-page semantic-extraction cache reuse is no
    // longer guaranteed. What must hold regardless of interleaving: each record
    // accounts for both chunks (hits + misses == chunks_total), each distinct
    // chunk hash is computed at least once (2..=4 total misses), and the cache
    // converges to exactly two idempotent rows.
    for counters in [&first_counters, &second_counters] {
        let hits = counters["cache_hits"].as_u64().unwrap_or(0);
        let misses = counters["cache_misses"].as_u64().unwrap_or(0);
        assert_eq!(hits + misses, 2, "each record accounts for both chunks");
    }
    let total_misses = first_counters["cache_misses"].as_u64().unwrap_or(0)
        + second_counters["cache_misses"].as_u64().unwrap_or(0);
    assert!(
        (2..=4).contains(&total_misses),
        "each distinct chunk hash is computed at least once, got {total_misses} misses"
    );
    let total_entities = first_counters["entities_extracted"].as_u64().unwrap_or(0)
        + second_counters["entities_extracted"].as_u64().unwrap_or(0);
    let total_relations = first_counters["relations_extracted"].as_u64().unwrap_or(0)
        + second_counters["relations_extracted"].as_u64().unwrap_or(0);
    let total_links = first_counters["semantic_chunk_links"].as_u64().unwrap_or(0)
        + second_counters["semantic_chunk_links"]
            .as_u64()
            .unwrap_or(0);
    assert!(total_entities > 0, "semantic entities are extracted");
    assert!(total_relations > 0, "semantic relations are extracted");
    assert!(
        total_links > 0,
        "same-document semantic chunk links are created"
    );
    assert_eq!(semantic_graph_cache_row_count(&pool, tenant_id).await, 2);

    let edge_json = graph.edge_properties_json();
    assert!(
        edge_json.contains("semantic_graph"),
        "semantic graph edge metadata should be written: {edge_json}"
    );
    assert!(
        edge_json.contains("RELATES_TO"),
        "same-document semantic chunk links should be graph-visible: {edge_json}"
    );
}

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

    repository
        .upsert_connection(KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: "test_provider".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "acct_structure_audit".to_string(),
            credential_ref: "vault://knowledge/structure-audit".to_string(),
            status: ConnectionStatus::Active,
            metadata: json!({}),
            source_selection: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
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
    assert!(
        chunks.iter().all(|chunk| chunk.graph_node_uid.is_some()),
        "{chunks:?}"
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

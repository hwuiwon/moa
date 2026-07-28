//! Semantic graph ingestion scenarios.

use moa_core::types::memory::SemanticGraphPolicy;
use moa_knowledge::semantic_graph::{SEMANTIC_GRAPH_MODEL, SEMANTIC_GRAPH_PROMPT_VERSION};

use super::*;

/// Reads the `(model, prompt_version)` identity of every cached extraction row
/// for a tenant, ordered for stable assertions.
async fn semantic_graph_cache_identities(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
) -> Vec<(String, String)> {
    sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT model, prompt_version
        FROM moa.knowledge_semantic_graph_extractions
        WHERE tenant_id = $1
        ORDER BY model, prompt_version
        "#,
    )
    .bind(tenant_id.0)
    .fetch_all(pool)
    .await
    .expect("read semantic graph cache identities")
}

#[tokio::test]
async fn disabled_semantic_policy_writes_no_semantic_rows_nodes_or_edges_db_memory() {
    // Pins: the whole point of Task 5.5. Under the default (off) policy, ingestion
    // pays NO semantic extraction cost and leaves NO semantic residue — no cache
    // rows, no semantic entity/relation edge properties — while the structural
    // document graph is still written. A regression that quietly resumes semantic
    // writes recreates exactly the write-only path this task removed, and no
    // count-based check on chunks or embeddings would notice.
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
        Arc::new(CountingEmbedder::default()),
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
        moa_knowledge::ingestion::KnowledgeSourceAclContext::for_capability(
            moa_knowledge::domain::ProviderAclCapability::UniformlyPublic,
        ),
    )
    .with_semantic_policy(SemanticGraphPolicy::Off);
    repository
        .upsert_connection(KnowledgeConnection {
            acl_mode: moa_knowledge::domain::ConnectionAclMode::TenantPublic,
            connection_uid,
            tenant_id,
            provider: "test_provider".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "acct_semantic_off".to_string(),
            credential_ref: "7bf8acf9-754e-7a67-b773-1ae68be8d3b8".to_string(),
            status: ConnectionStatus::Active,
            metadata: json!({}),
            source_selection: json!({}),
            information_barrier: None,
            created_at: moa_test_support::fixtures::pg_now(),
            updated_at: moa_test_support::fixtures::pg_now(),
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
                records: vec![record_with_source("semantic-off", "v1", false, text)],
                next_cursor: None,
            },
        )
        .await
        .expect("ingest record with semantic graph disabled");

    assert_eq!(result.records_ingested, 1);
    let object_uid = object_uid_for_source(connection_uid, "semantic-off");
    let counters = semantic_graph_step_counters(&pool, sync_run_uid, object_uid).await;
    // The document was chunked, so the step still runs and reports its work; what
    // must be zero is every semantic quantity.
    assert!(
        counters["chunks_total"].as_u64().unwrap_or(0) > 0,
        "the document is still chunked and ingested: {counters}"
    );
    for zeroed in [
        "cache_hits",
        "cache_misses",
        "entities_extracted",
        "relations_extracted",
        "semantic_chunk_links",
    ] {
        assert_eq!(
            counters[zeroed].as_u64().unwrap_or(0),
            0,
            "{zeroed} must be zero under a disabled semantic policy: {counters}"
        );
    }
    assert!(
        semantic_graph_cache_identities(&pool, tenant_id)
            .await
            .is_empty(),
        "a disabled policy writes no extraction cache rows"
    );
    let edge_json = graph.edge_properties_json();
    assert!(
        !edge_json.contains("semantic_graph"),
        "no semantic graph edge metadata may be written: {edge_json}"
    );
    // The structural document graph is unaffected: this task removed semantic
    // writes, not knowledge ingestion.
    assert!(
        edge_json.contains("HAS_CHUNK"),
        "structural document/chunk edges are still written: {edge_json}"
    );
}

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
        moa_knowledge::ingestion::KnowledgeSourceAclContext::for_capability(
            moa_knowledge::domain::ProviderAclCapability::UniformlyPublic,
        ),
    )
    .with_semantic_policy(SemanticGraphPolicy::Deterministic);
    repository
        .upsert_connection(KnowledgeConnection {
            acl_mode: moa_knowledge::domain::ConnectionAclMode::TenantPublic,
            connection_uid,
            tenant_id,
            provider: "test_provider".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "acct_semantic".to_string(),
            credential_ref: "7bf8acf9-754e-7a67-b773-1ae68be8d3b8".to_string(),
            status: ConnectionStatus::Active,
            metadata: json!({}),
            source_selection: json!({}),
            information_barrier: None,
            created_at: moa_test_support::fixtures::pg_now(),
            updated_at: moa_test_support::fixtures::pg_now(),
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
    // Cache rows carry the deterministic ruleset's own identity rather than a
    // generic label, so revising the ruleset re-extracts instead of serving rows
    // produced by the previous one.
    let identities = semantic_graph_cache_identities(&pool, tenant_id).await;
    assert!(
        identities.iter().all(|(model, prompt_version)| {
            model == SEMANTIC_GRAPH_MODEL && prompt_version == SEMANTIC_GRAPH_PROMPT_VERSION
        }),
        "every cached row carries the deterministic ruleset identity: {identities:?}"
    );

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
async fn generic_entity_fallback_writes_graph_entities_on_general_corpus_db_memory() {
    // Pins: on a general-corpus document with no domain-rule match, the generic
    // proper-noun fallback still emits Entity nodes and MENTIONS edges, so the
    // graph retrieval leg has nodes to seed on arbitrary corpora.
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
                target_tokens: 32,
                max_tokens: 64,
                min_tokens: 1,
            },
            provider: "test_provider".to_string(),
            parser_label: "test_parser".to_string(),
        },
        moa_knowledge::ingestion::KnowledgeSourceAclContext::for_capability(
            moa_knowledge::domain::ProviderAclCapability::UniformlyPublic,
        ),
    )
    .with_semantic_policy(SemanticGraphPolicy::Deterministic);
    repository
        .upsert_connection(KnowledgeConnection {
            acl_mode: moa_knowledge::domain::ConnectionAclMode::TenantPublic,
            connection_uid,
            tenant_id,
            provider: "test_provider".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "acct_generic".to_string(),
            credential_ref: "77903500-8e95-6ed7-9053-77c0dcc70fb8".to_string(),
            status: ConnectionStatus::Active,
            metadata: json!({}),
            source_selection: json!({}),
            information_barrier: None,
            created_at: moa_test_support::fixtures::pg_now(),
            updated_at: moa_test_support::fixtures::pg_now(),
            last_synced_at: None,
        })
        .await
        .expect("upsert connection");
    // No Wix support phrases or requirement keywords, so the domain ruleset
    // matches nothing and only the generic fallback can produce entities.
    let text = "Barack Obama met Angela Merkel in Berlin during the Geneva Summit.";
    let sync_run_uid = create_run(&repository, tenant_id, connection_uid).await;

    let result = pipeline
        .ingest_record_page(
            sync_run_uid,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record_with_source("generic-a", "v1", false, text)],
                next_cursor: None,
            },
        )
        .await
        .expect("ingest general-corpus record");

    assert_eq!(result.records_ingested, 1);
    let object_uid = object_uid_for_source(connection_uid, "generic-a");
    let counters = semantic_graph_step_counters(&pool, sync_run_uid, object_uid).await;
    assert!(
        counters["entities_extracted"].as_u64().unwrap_or(0) > 0,
        "generic fallback extracts entities on general text: {counters}"
    );

    let node_json = graph.properties_json();
    assert!(
        node_json.contains("Barack Obama"),
        "a generic proper-noun Entity node is written: {node_json}"
    );
    assert!(
        node_json.contains("semantic_graph_extraction"),
        "generic entities flow through the semantic extraction node path: {node_json}"
    );
    let edge_json = graph.edge_properties_json();
    assert!(
        edge_json.contains("MENTIONS"),
        "chunk -> entity MENTIONS edges are graph-visible: {edge_json}"
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
        moa_knowledge::ingestion::KnowledgeSourceAclContext::for_capability(
            moa_knowledge::domain::ProviderAclCapability::UniformlyPublic,
        ),
    )
    .with_semantic_policy(SemanticGraphPolicy::Deterministic);

    repository
        .upsert_connection(KnowledgeConnection {
            acl_mode: moa_knowledge::domain::ConnectionAclMode::TenantPublic,
            connection_uid,
            tenant_id,
            provider: "test_provider".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "acct_structure_audit".to_string(),
            credential_ref: "48344059-1fe3-e088-283b-d2f3d3a66d08".to_string(),
            status: ConnectionStatus::Active,
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

//! Trace behavior for the tenant Knowledge service.

use super::*;

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
    // A tenant-knowledge chunk hit's graph node uid IS its chunk occurrence uid;
    // the graph path that reached it starts at the document node.
    let document_node_uid = Uuid::now_v7();
    let chunk_uid = Uuid::now_v7();
    let event = LineageEvent::Retrieval(RetrievalLineage {
        turn_id,
        session_id,
        storage_partition_id: storage_partition_id.clone(),
        user_id: UserId::new("query-trace-user"),
        scope: MemoryScope::Tenant { tenant_id },
        ts: moa_test_support::fixtures::pg_now(),
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
            start: document_node_uid,
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
            graph_node_uid: chunk_uid,
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
    .bind(moa_test_support::fixtures::pg_now())
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

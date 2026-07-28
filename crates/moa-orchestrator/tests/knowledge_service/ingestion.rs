//! Ingestion behavior for the tenant Knowledge service.

use super::*;

#[tokio::test]
async fn knowledge_auto_sync_manual_sync_triggers_provider_and_does_not_ingest_inline() {
    // Pins: manual sync returns after provider trigger and only touches sync-run state.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let caller = test_caller(tenant_id);
    let connection = fixture_connection(tenant_id);
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    repository
        .insert_connection(connection.clone())
        .expect("fixture connection should be inserted");
    let provider = Arc::new(FakeLinkedIntegrationProvider::default());
    let service = fixture_service(repository.clone(), provider.clone(), 80);

    let response = service
        .sync_connection(
            KnowledgeSyncRequest {
                tenant_id,
                connection_uid: connection.connection_uid,
                parser: Some("native".to_string()),
                max_records: Some(25),
            },
            &caller,
        )
        .await
        .expect("manual sync should trigger provider sync");

    assert_eq!(response.status, "provider_syncing");
    assert_eq!(provider.trigger_sync_count(), 1);
    assert_eq!(provider.list_changed_records_count(), 0);
    assert_eq!(repository.op_count("claim_sync_run"), 1);
    assert_eq!(repository.op_count("update_sync_run"), 1);
    assert_eq!(repository.op_count("record_ingestion_step"), 1);
    assert_eq!(repository.op_count("upsert_object"), 0);
    assert_eq!(repository.op_count("insert_document_version"), 0);
    assert_eq!(repository.op_count("replace_blocks"), 0);
    assert_eq!(repository.op_count("replace_chunks"), 0);
    assert_eq!(repository.op_count("add_sync_counters"), 0);
    assert_eq!(repository.sync_run_count(), 1);
    assert_eq!(repository.step_count(), 1);
}

#[tokio::test]
async fn knowledge_auto_sync_manual_sync_immediate_provider_completion_marks_run_ready_for_workflow()
 {
    // Pins: immediate provider completion marks the run provider-synced and records the same ingestion enqueue marker used by webhooks.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let caller = test_caller(tenant_id);
    let connection = fixture_connection(tenant_id);
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    repository
        .insert_connection(connection.clone())
        .expect("fixture connection should be inserted");
    let provider = Arc::new(FakeLinkedIntegrationProvider::with_trigger_status(
        "completed",
    ));
    let service = fixture_service(repository.clone(), provider.clone(), 80);

    let response = service
        .sync_connection(
            KnowledgeSyncRequest {
                tenant_id,
                connection_uid: connection.connection_uid,
                parser: Some("native".to_string()),
                max_records: Some(25),
            },
            &caller,
        )
        .await
        .expect("manual sync should accept an immediate provider completion");

    assert_eq!(response.status, "provider_synced");
    assert_eq!(provider.trigger_sync_count(), 1);
    assert_eq!(provider.list_changed_records_count(), 0);
    assert_eq!(repository.op_count("claim_sync_run"), 1);
    assert_eq!(repository.op_count("update_sync_run"), 1);
    assert_eq!(repository.op_count("record_ingestion_step"), 1);
    assert_eq!(repository.op_count("record_ingestion_step_once"), 1);
    assert_eq!(repository.sync_run_count(), 1);
    assert_eq!(repository.step_count(), 2);
}

#[tokio::test]
async fn mock_connector_end_to_end_db_memory() {
    // Pins: fake Merge and Nango connector syncs can be manually driven through tenant KB ingestion and inspected without external credentials.
    let db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated mock connector DB");
    let pool = db.store().pool().clone();
    let kms: Arc<dyn moa_crypto::KeyManagementProvider> =
        Arc::new(moa_crypto::LocalKmsProvider::new());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let caller = test_caller(tenant_id);
    let contact_id = ContactId::new();
    let information_barrier =
        InformationBarrierId::parse("knowledge-restricted").expect("valid barrier");
    let merge_provider = Arc::new(Task14LinkedIntegrationProvider::new(
        "merge",
        "crm",
        task14_merge_records(),
    ));
    let nango_provider = Arc::new(Task14LinkedIntegrationProvider::new(
        "nango",
        "docs",
        task14_nango_records(),
    ));
    let providers = StaticKnowledgeProviders::new()
        .with_provider("merge", merge_provider.clone())
        .with_provider("nango", nango_provider.clone());
    let service = KnowledgeService::from_postgres_pool(
        pool.clone(),
        Arc::new(providers),
        Arc::new(FakeKnowledgeCredentialStore::default()),
        fake_ingestion_runner(),
        96,
    );

    let merge_connection = service
        .exchange_public_token(
            KnowledgeExchangeTokenRequest {
                tenant_id,
                provider: "merge".to_string(),
                connector: CONNECTOR.to_string(),
                exchange_token: "merge-public-token".to_string(),
                source_selection: json!({}),
                information_barrier: Some(information_barrier.clone()),
            },
            &caller,
        )
        .await
        .expect("merge link should store one fake connection");
    // A distinct link is a distinct operation: each Restate invocation mints its
    // own replay-stable id, so reusing one here would be a fenced conflict.
    let nango_caller = test_caller(tenant_id);
    let nango_connection = service
        .exchange_public_token(
            KnowledgeExchangeTokenRequest {
                tenant_id,
                provider: "nango".to_string(),
                connector: CONNECTOR.to_string(),
                exchange_token: "nango-public-token".to_string(),
                source_selection: json!({}),
                information_barrier: Some(information_barrier.clone()),
            },
            &nango_caller,
        )
        .await
        .expect("nango link should store one fake connection");
    assert_ne!(
        merge_connection.connection_uid,
        nango_connection.connection_uid
    );

    let merge_sync = service
        .sync_connection(
            KnowledgeSyncRequest {
                tenant_id,
                connection_uid: merge_connection.connection_uid,
                parser: Some("task14".to_string()),
                max_records: Some(10),
            },
            &caller,
        )
        .await
        .expect("merge manual sync should trigger provider sync");
    let nango_sync = service
        .sync_connection(
            KnowledgeSyncRequest {
                tenant_id,
                connection_uid: nango_connection.connection_uid,
                parser: Some("task14".to_string()),
                max_records: Some(10),
            },
            &caller,
        )
        .await
        .expect("nango manual sync should trigger provider sync");
    assert_eq!(merge_sync.status, "provider_syncing");
    assert_eq!(nango_sync.status, "provider_syncing");
    // Each link already started its own initial sync, and a manual sync while
    // that run still holds the connection's active slot returns it rather than
    // dispatching a second provider call.
    assert_eq!(merge_provider.start_initial_sync_count(), 1);
    assert_eq!(nango_provider.start_initial_sync_count(), 1);
    assert_eq!(merge_provider.trigger_sync_count(), 0);
    assert_eq!(nango_provider.trigger_sync_count(), 0);

    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope.clone(),
    ));
    seed_task14_embedder_state(&pool, tenant_id).await;
    let graph_scope = scope
        .clone()
        .with_cleared_barriers([information_barrier.clone()].into_iter().collect());
    let vector = Arc::new(PgvectorStore::new_for_app_role(
        pool.clone(),
        graph_scope.clone(),
    ));
    let graph_store = Arc::new(
        PostgresGraphStore::scoped_for_app_role(pool.clone(), graph_scope, kms.clone())
            .with_vector_store(vector),
    );
    let graph_writer = Arc::new(MemoryKnowledgeGraphWriter::new(
        graph_store.clone(),
        MemoryScope::Tenant { tenant_id },
        "task14-mock-connector",
        Some(information_barrier.clone()),
    ));
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        Arc::new(Task14Parser),
        Arc::new(Task14Embedder),
        graph_writer,
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 128,
                max_tokens: 256,
                min_tokens: 1,
            },
            provider: "mock_connector".to_string(),
            parser_label: "task14".to_string(),
        },
        moa_knowledge::ingestion::KnowledgeSourceAclContext::for_capability(
            moa_knowledge::domain::ProviderAclCapability::UniformlyPublic,
        ),
    )
    // Opts into the semantic-enabled policy for the same reason
    // `task14_ingestion_pipeline` does: this test pins the graph node/edge
    // counters, and the full write set is the interesting one to pin. Under the
    // default (off) policy the same three records per connector produce 16/13
    // and 15/12 — no semantic entity or edge at all — which is pinned separately
    // by `disabled_semantic_policy_writes_no_semantic_rows_nodes_or_edges_db_memory`
    // in moa-knowledge.
    .with_semantic_policy(moa_core::types::memory::SemanticGraphPolicy::Deterministic);

    let merge_connection_row = repository
        .get_connection(merge_connection.connection_uid)
        .await
        .expect("read merge connection")
        .expect("merge connection should exist");
    let nango_connection_row = repository
        .get_connection(nango_connection.connection_uid)
        .await
        .expect("read nango connection")
        .expect("nango connection should exist");
    let merge_page = merge_provider
        .list_changed_records(ListChangedRecordsRequest {
            acl_key: std::sync::Arc::new(moa_knowledge::acl_key::SourceAclKey::new(1, vec![7; 32])),
            connection: merge_connection_row,
            credential: RedactedSecret::new("merge-provider-credential".to_string()),
            cursor: None,
            modified_after: None,
            limit: Some(10),
            variant: None,
        })
        .await
        .expect("merge fake provider should return changed records");
    let nango_page = nango_provider
        .list_changed_records(ListChangedRecordsRequest {
            acl_key: std::sync::Arc::new(moa_knowledge::acl_key::SourceAclKey::new(1, vec![7; 32])),
            connection: nango_connection_row,
            credential: RedactedSecret::new("nango-provider-credential".to_string()),
            cursor: None,
            modified_after: None,
            limit: Some(10),
            variant: None,
        })
        .await
        .expect("nango fake provider should return changed records");
    assert_eq!(merge_provider.list_changed_records_count(), 1);
    assert_eq!(nango_provider.list_changed_records_count(), 1);
    assert_eq!(merge_page.records.len(), 3);
    assert_eq!(nango_page.records.len(), 3);

    pipeline
        .ingest_record_page(
            merge_sync.sync_run_uid,
            merge_connection.connection_uid,
            tenant_id,
            merge_page,
        )
        .await
        .expect("merge fake records should ingest");
    pipeline
        .ingest_record_page(
            nango_sync.sync_run_uid,
            nango_connection.connection_uid,
            tenant_id,
            nango_page,
        )
        .await
        .expect("nango fake records should ingest");
    let merge_run = repository
        .get_sync_run(merge_sync.sync_run_uid)
        .await
        .expect("read merge sync run")
        .expect("merge sync run should exist");
    assert_eq!(
        merge_run.information_barrier,
        Some(information_barrier.clone()),
        "the run must retain the connection barrier snapshot"
    );
    let (node_count, tagged_count): (i64, i64) = sqlx::query_as(
        r#"
        SELECT count(*), count(*) FILTER (WHERE barrier = $2)
        FROM moa.node_index
        WHERE tenant_id = $1
          AND valid_to IS NULL
        "#,
    )
    .bind(tenant_id.0)
    .bind(information_barrier.as_str())
    .fetch_one(&pool)
    .await
    .expect("count barrier-tagged knowledge nodes");
    assert!(
        node_count > 0,
        "knowledge ingestion should create graph nodes"
    );
    assert_eq!(
        tagged_count, node_count,
        "every knowledge-sync node must carry the run's authoritative barrier"
    );
    let node_uids = sqlx::query_scalar::<_, Uuid>(
        "SELECT uid FROM moa.node_index WHERE tenant_id = $1 AND valid_to IS NULL ORDER BY uid",
    )
    .bind(tenant_id.0)
    .fetch_all(&pool)
    .await
    .expect("load knowledge node UIDs");
    let uncleared_graph = PostgresGraphStore::scoped_for_app_role(pool.clone(), scope, kms.clone());
    assert!(
        uncleared_graph
            .bulk_get_nodes(&node_uids)
            .await
            .expect("read knowledge nodes without clearance")
            .is_empty(),
        "knowledge nodes must fail closed without the source barrier clearance"
    );
    let cleared_graph = PostgresGraphStore::scoped_for_app_role(
        pool.clone(),
        RlsContext::tenant(tenant_id)
            .with_cleared_barriers([information_barrier.clone()].into_iter().collect()),
        kms,
    );
    assert_eq!(
        cleared_graph
            .bulk_get_nodes(&node_uids)
            .await
            .expect("read knowledge nodes with clearance")
            .len(),
        node_uids.len(),
        "source clearance must reveal all nodes from the sync run"
    );
    complete_sync_run(&repository, merge_sync.sync_run_uid)
        .await
        .expect("complete merge sync run");
    complete_sync_run(&repository, nango_sync.sync_run_uid)
        .await
        .expect("complete nango sync run");

    let account_object = repository
        .get_object_by_source(merge_connection.connection_uid, "merge-crm-account")
        .await
        .expect("read account object")
        .expect("account object should be ingested");
    let group_delta =
        derive_contact_groups_from_object_with_resolved_members(&account_object, &[contact_id]);
    assert_eq!(group_delta.groups.len(), 1);
    assert_eq!(group_delta.memberships.len(), 1);
    let group = group_delta
        .groups
        .first()
        .expect("group should be derived")
        .clone();
    repository
        .upsert_contact_group(group.clone())
        .await
        .expect("persist derived contact group");
    repository
        .replace_contact_group_memberships(group.group_uid, group_delta.memberships)
        .await
        .expect("persist derived group membership");
    let group_node_uid = create_contact_group_graph_node(&graph_store, tenant_id, &group)
        .await
        .expect("materialize contact group graph node");
    assert_ne!(group_node_uid, Uuid::nil());

    let merge_status = service
        .sync_status(KnowledgeSyncStatusRequest {
            tenant_id,
            sync_run_uid: merge_sync.sync_run_uid,
        })
        .await
        .expect("merge status should render");
    let nango_status = service
        .sync_status(KnowledgeSyncStatusRequest {
            tenant_id,
            sync_run_uid: nango_sync.sync_run_uid,
        })
        .await
        .expect("nango status should render");
    // Counts are for the semantic-enabled policy this pipeline opts into above.
    // Of these, the semantic write set is 5 nodes / 5 edges per connector: drop
    // the opt-in and the same records produce 16/13 and 15/12.
    //
    // The deterministic generic proper-noun fallback emits one extra Entity node
    // + link edge for each chunk whose
    // text concatenates a bare heading with its body, producing a capitalized
    // span: merge's "PTO Policy PTO" (+1 node/+1 edge), and nango's
    // "Finance Controls Finance" and "Support Guide Support" (+2 nodes/+2 edges).
    // The reducto/warehouse chunk carries no heading line so it stays unchanged.
    assert_sync_status_counters(&merge_status, 3, 21, 18);
    assert_sync_status_counters(&nango_status, 3, 20, 17);
    assert_eq!(
        merge_status
            .steps
            .iter()
            .take(2)
            .map(|step| step.step.as_str())
            .collect::<Vec<_>>(),
        vec!["provider_triggered", "provider_records_listed"]
    );
    assert_eq!(
        nango_status
            .steps
            .iter()
            .take(2)
            .map(|step| step.step.as_str())
            .collect::<Vec<_>>(),
        vec!["provider_triggered", "provider_records_listed"]
    );

    let objects = service
        .list_objects(KnowledgeObjectListRequest {
            tenant_id,
            connection_uid: None,
            object_type: None,
            cursor: None,
            limit: Some(10),
        })
        .await
        .expect("object summaries should render");
    assert_eq!(objects.objects.len(), 6);
    let mut object_source_ids = objects
        .objects
        .iter()
        .map(|object| {
            assert_eq!(object["parser_status"], json!("parsed"));
            assert_eq!(object["chunk_count"], json!(1));
            object["source_id"]
                .as_str()
                .expect("object summary should include source_id")
                .to_string()
        })
        .collect::<Vec<_>>();
    object_source_ids.sort();
    assert_eq!(
        object_source_ids,
        vec![
            "merge-crm-account",
            "merge-crm-contact",
            "merge-md-handbook",
            "nango-llamaparse-policy",
            "nango-reducto-layout",
            "nango-unstructured-guide",
        ]
    );

    let llama_object = repository
        .get_object_by_source(nango_connection.connection_uid, "nango-llamaparse-policy")
        .await
        .expect("read llama object")
        .expect("llama object should exist");
    let llama_inspect = service
        .inspect_object(KnowledgeObjectInspectRequest {
            tenant_id,
            object_uid: llama_object.object_uid,
        })
        .await
        .expect("llamaparse object should inspect");
    assert_eq!(llama_inspect.parser.as_deref(), Some("llamaparse"));
    assert_eq!(
        llama_inspect.parser_metadata["job_status"],
        json!("completed")
    );
    assert_eq!(llama_inspect.chunks.len(), 1);
    assert!(
        llama_inspect.chunks[0]
            .preview
            .contains("Finance control is")
    );
    assert_eq!(
        llama_inspect
            .steps
            .iter()
            .map(|step| step.step.as_str())
            .collect::<Vec<_>>(),
        object_ingestion_steps()
    );

    let reducto_object = repository
        .get_object_by_source(nango_connection.connection_uid, "nango-reducto-layout")
        .await
        .expect("read reducto object")
        .expect("reducto object should exist");
    let reducto_inspect = service
        .inspect_object(KnowledgeObjectInspectRequest {
            tenant_id,
            object_uid: reducto_object.object_uid,
        })
        .await
        .expect("reducto object should inspect");
    assert_eq!(reducto_inspect.parser.as_deref(), Some("reducto"));
    assert_eq!(
        reducto_inspect.parser_metadata["blocks"][0]["bbox"],
        json!([0.1, 0.2, 0.7, 0.4])
    );

    let trace_uid = Uuid::now_v7();
    let trace_chunk = llama_inspect
        .chunks
        .first()
        .expect("llamaparse object should expose one chunk");
    // A chunk's occurrence identity IS its graph node uid.
    let trace_graph_uid = trace_chunk.chunk_uid;
    let contact_fact_uid = Uuid::now_v7();
    let retrieval_event = LineageEvent::Retrieval(RetrievalLineage {
        turn_id: TurnId(trace_uid),
        session_id: SessionId::new(),
        storage_partition_id: StoragePartitionId::for_tenant(tenant_id),
        user_id: UserId::new(contact_id.to_string()),
        scope: MemoryScope::Contact {
            tenant_id,
            contact_id,
        },
        ts: moa_test_support::fixtures::pg_now(),
        query_original: "Where is the finance payroll control?".to_string(),
        query_expansions: vec!["finance payroll control".to_string()],
        vector_hits: vec![VecHit {
            chunk_id: trace_graph_uid,
            score: 0.91,
            source: "pgvector".to_string(),
            embedder: "embed-v4.0".to_string(),
            embed_dim: VECTOR_DIMENSION as u16,
        }],
        graph_paths: vec![GraphPath {
            start: trace_graph_uid,
            end: trace_graph_uid,
            edges: Vec::new(),
            labels: vec!["HAS_CHUNK".to_string()],
            length: 0,
            score: 0.88,
        }],
        fusion_scores: vec![
            FusedHit {
                chunk_id: trace_graph_uid,
                fused_score: 0.94,
                vector_contribution: 1.0,
                graph_contribution: 1.0,
                lexical_contribution: 1.0,
                fusion_method: "rrf".to_string(),
            },
            FusedHit {
                chunk_id: contact_fact_uid,
                fused_score: 0.72,
                vector_contribution: 0.0,
                graph_contribution: 0.0,
                lexical_contribution: 1.0,
                fusion_method: "rrf".to_string(),
            },
        ],
        rerank_scores: vec![
            RerankHit {
                chunk_id: trace_graph_uid,
                original_index: 0,
                relevance_score: 0.97,
                rerank_model: "noop".to_string(),
            },
            RerankHit {
                chunk_id: contact_fact_uid,
                original_index: 1,
                relevance_score: 0.76,
                rerank_model: "noop".to_string(),
            },
        ],
        top_k: vec![trace_graph_uid, contact_fact_uid],
        searched_scopes: vec![
            format!("tenant:{tenant_id}:tenant_knowledge"),
            format!("contact:{tenant_id}:{contact_id}:user_memory"),
        ],
        selected_hits: vec![
            RetrievalSelectedHit {
                graph_node_uid: trace_graph_uid,
                chunk_uid: Some(trace_chunk.chunk_uid),
                fact_uid: None,
                source_tier: "tenant_knowledge".to_string(),
                label: "Chunk".to_string(),
                title: "Finance Controls".to_string(),
                snippet: trace_chunk.preview.clone(),
                score: 0.97,
                legs: vec![
                    "vector".to_string(),
                    "graph".to_string(),
                    "lexical".to_string(),
                ],
                prompt_included: true,
                source_uri: Some("https://nango.example.test/docs/finance-controls".to_string()),
                source_title: Some("Finance Controls".to_string()),
                citation: json!({
                    "chunk_hash": trace_chunk.chunk_hash.clone(),
                    "heading_path": trace_chunk.heading_path.clone(),
                    "object_type": "document",
                }),
            },
            RetrievalSelectedHit {
                graph_node_uid: contact_fact_uid,
                chunk_uid: None,
                fact_uid: Some(contact_fact_uid),
                source_tier: "user_memory".to_string(),
                label: "Fact".to_string(),
                title: "Contact preference".to_string(),
                snippet: "Contact prefers payroll reminders before approval.".to_string(),
                score: 0.76,
                legs: vec!["lexical".to_string()],
                prompt_included: true,
                source_uri: None,
                source_title: None,
                citation: json!({}),
            },
        ],
        filters: json!({
            "source_tiers": ["tenant_knowledge", "user_memory"],
            "tenant_knowledge_labels": ["Chunk", "ContactGroup"],
        }),
        timings: StageTimings {
            embed_ms: 2,
            vector_search_ms: 4,
            graph_search_ms: 6,
            lexical_search_ms: 3,
            fusion_ms: 1,
            rerank_ms: 2,
            total_ms: 21,
        },
        introspection: BackendIntrospection::default(),
        stage: RetrievalStage::Single,
    });
    insert_retrieval_lineage_row(&pool, retrieval_event, trace_uid, tenant_id)
        .await
        .expect("persist task14 retrieval lineage");
    let query_trace = service
        .query_trace(KnowledgeQueryTraceRequest {
            tenant_id,
            trace_uid,
        })
        .await
        .expect("query trace should render task14 retrieval lineage");
    assert_eq!(
        query_trace.original_query,
        "Where is the finance payroll control?"
    );
    assert_eq!(
        query_trace.retrieval_query.as_deref(),
        Some("finance payroll control")
    );
    assert_eq!(
        query_trace.searched_scopes,
        vec![
            format!("tenant:{tenant_id}:tenant_knowledge"),
            format!("contact:{tenant_id}:{contact_id}:user_memory"),
        ]
    );
    assert_eq!(
        query_trace
            .stages
            .iter()
            .map(|stage| (
                stage.stage.as_str(),
                stage.candidate_count,
                stage.latency_ms
            ))
            .collect::<Vec<_>>(),
        vec![
            ("embed", 0, 2),
            ("vector", 1, 4),
            ("graph", 1, 6),
            ("lexical", 2, 3),
            ("fusion", 2, 1),
            ("reranker", 2, 2),
            ("context", 2, 21),
        ]
    );
    assert_eq!(query_trace.hits.len(), 2);
    assert_eq!(query_trace.hits[0].source_tier, "tenant_knowledge");
    assert_eq!(
        query_trace.hits[0].citation["chunk_hash"],
        json!(trace_chunk.chunk_hash.clone())
    );
    assert_eq!(
        query_trace.hits[0].citation["legs"],
        json!(["vector", "graph", "lexical"])
    );
    assert_eq!(
        query_trace.hits[0].citation["source_uri"],
        json!("https://nango.example.test/docs/finance-controls")
    );
    assert_eq!(query_trace.hits[1].source_tier, "user_memory");

    let merge_events = service
        .sync_events(KnowledgeSyncEventsRequest {
            tenant_id,
            sync_run_uid: merge_sync.sync_run_uid,
            object_uid: Some(account_object.object_uid),
            cursor: None,
            limit: Some(20),
        })
        .await
        .expect("object sync events should render");
    assert_eq!(
        merge_events
            .events
            .iter()
            .map(|step| step.step.as_str())
            .collect::<Vec<_>>(),
        object_ingestion_steps()
    );

    let label_counts = graph_label_counts(&pool, tenant_id).await;
    assert_eq!(label_counts.get("Source"), Some(&6));
    assert_eq!(label_counts.get("Document"), Some(&6));
    assert_eq!(label_counts.get("Chunk"), Some(&6));
    assert_eq!(label_counts.get("Fact"), Some(&6));
    // 14 curated title/heading/domain entities plus 3 generic proper-noun spans
    // from the heading-into-body fallback ("PTO Policy PTO", "Finance Controls
    // Finance", "Support Guide Support").
    assert_eq!(label_counts.get("Entity"), Some(&17));
    assert_eq!(label_counts.get("ContactGroup"), Some(&1));
    assert_eq!(chunk_vector_row_count(&pool, tenant_id).await, 6);

    let target = repository
        .contact_group_targets(tenant_id, &group.group_key)
        .await
        .expect("load derived target group")
        .expect("target group should exist");
    assert_eq!(
        target.group.group_key,
        format!(
            "merge:{}:account:acct-task14",
            merge_connection.connection_uid
        )
    );
    assert_eq!(
        target
            .members
            .iter()
            .map(|member| member.contact_id)
            .collect::<Vec<_>>(),
        vec![contact_id]
    );
    assert_eq!(target.active_graph_memberships.len(), 1);
    assert_eq!(target.active_graph_memberships[0].edge_label, "MEMBER_OF");
    assert_eq!(
        target.active_graph_memberships[0].evidence,
        vec![account_object.object_uid]
    );
}

#[tokio::test]
async fn knowledge_auto_sync_provider_synced_run_lists_changed_records_and_ingests_db_memory() {
    // Pins: a provider-synced run lists changed records with its cursor/limit/watermark and applies them to tenant graph/vector knowledge.
    let db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated knowledge auto-sync DB");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let modified_after = moa_test_support::fixtures::pg_now();
    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope,
    ));
    repository
        .upsert_connection(KnowledgeConnection {
            acl_mode: moa_knowledge::domain::ConnectionAclMode::TenantPublic,
            connection_uid,
            tenant_id,
            provider: "nango".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "nango-task14-account".to_string(),
            credential_ref: "c42bc21d-9469-aa8a-2667-39711cae3cb1".to_string(),
            status: ConnectionStatus::Active,
            metadata: json!({ "safe": "connection" }),
            source_selection: json!({}),
            information_barrier: None,
            created_at: moa_test_support::fixtures::pg_now(),
            updated_at: moa_test_support::fixtures::pg_now(),
            last_synced_at: Some(modified_after),
        })
        .await
        .expect("seed Nango knowledge connection");
    let sync_run_uid =
        create_provider_synced_run(&repository, tenant_id, connection_uid, Some(2)).await;
    let provider = Arc::new(Task14LinkedIntegrationProvider::new(
        "nango",
        "docs",
        task14_nango_records(),
    ));
    seed_task14_embedder_state(&pool, tenant_id).await;
    let pipeline = task14_ingestion_pipeline(pool.clone(), repository.clone(), tenant_id, "nango");
    let mut steps =
        DbKnowledgeAutoSyncSteps::new(repository.clone(), provider.clone(), pipeline, 2, "task14");

    let report = run_knowledge_sync_ingestion_workflow(
        &mut steps,
        KnowledgeSyncIngestionRequest { sync_run_uid },
    )
    .await
    .expect("provider-synced run should auto-ingest changed records");

    assert_eq!(report.status, "completed");
    assert_eq!(report.records_listed, 2);
    assert_eq!(report.records_applied, 2);
    assert_eq!(report.records_pruned, 0);
    assert_eq!(
        provider.list_changed_record_requests(),
        vec![FakeListChangedRecordsRequest {
            connection_uid,
            cursor: None,
            limit: Some(2),
            modified_after: Some(modified_after),
            variant: None,
        }]
    );

    let run = repository
        .get_sync_run(sync_run_uid)
        .await
        .expect("read completed sync run")
        .expect("completed sync run should exist");
    assert_eq!(run.status, SyncRunStatus::Completed);
    assert_eq!(run.records_seen, 2);
    assert_eq!(run.records_changed, 2);
    assert_eq!(run.records_deleted, 0);
    assert_eq!(run.records_ingested, 2);
    assert_eq!(run.records_failed, 0);
    assert_eq!(run.objects_parsed, 2);
    assert_eq!(run.chunks_embedded, 2);
    assert!(run.graph_nodes_upserted > 0);
    assert!(run.graph_edges_upserted > 0);
    let updated_connection = repository
        .get_connection(connection_uid)
        .await
        .expect("read updated connection")
        .expect("connection should still exist");
    assert!(
        updated_connection.last_synced_at >= Some(modified_after),
        "completion should advance the connection sync watermark"
    );

    let mut source_ids = repository
        .list_objects(tenant_id, Some(connection_uid), None, 10)
        .await
        .expect("list ingested objects")
        .into_iter()
        .map(|object| object.object.source_id)
        .collect::<Vec<_>>();
    source_ids.sort();
    assert_eq!(
        source_ids,
        vec![
            "nango-llamaparse-policy".to_string(),
            "nango-unstructured-guide".to_string(),
        ]
    );
    let steps = repository
        .sync_run_steps(sync_run_uid, None)
        .await
        .expect("read sync steps");
    assert_eq!(
        steps
            .iter()
            .filter(|step| step.step == "provider_records_listed")
            .count(),
        1
    );
    assert_eq!(
        steps
            .iter()
            .filter(|step| step.step == "object_change_checked")
            .count(),
        2
    );
    assert_eq!(
        steps
            .iter()
            .filter(|step| step.step == "graph_upserted")
            .count(),
        2
    );
    let label_counts = graph_label_counts(&pool, tenant_id).await;
    assert_eq!(label_counts.get("Source"), Some(&2_i64));
    assert_eq!(label_counts.get("Document"), Some(&2_i64));
    assert_eq!(label_counts.get("Chunk"), Some(&2_i64));
    assert_eq!(label_counts.get("Fact"), Some(&2_i64));
    assert_eq!(chunk_vector_row_count(&pool, tenant_id).await, 2);
}

#[tokio::test]
async fn knowledge_auto_sync_record_listing_failure_marks_sync_retryable_db_memory() {
    // Pins: provider record-listing failures mark the DB sync run retryable without applying any records.
    let db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated knowledge auto-sync failure DB");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let modified_after = moa_test_support::fixtures::pg_now();
    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope,
    ));
    repository
        .upsert_connection(KnowledgeConnection {
            acl_mode: moa_knowledge::domain::ConnectionAclMode::TenantPublic,
            connection_uid,
            tenant_id,
            provider: "nango".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "nango-task14-account".to_string(),
            credential_ref: "c42bc21d-9469-aa8a-2667-39711cae3cb1".to_string(),
            status: ConnectionStatus::Active,
            metadata: json!({ "safe": "connection" }),
            source_selection: json!({}),
            information_barrier: None,
            created_at: moa_test_support::fixtures::pg_now(),
            updated_at: moa_test_support::fixtures::pg_now(),
            last_synced_at: Some(modified_after),
        })
        .await
        .expect("seed Nango knowledge connection");
    let sync_run_uid =
        create_provider_synced_run(&repository, tenant_id, connection_uid, Some(5)).await;
    let provider = Arc::new(Task14LinkedIntegrationProvider::failing_list(
        "nango",
        "docs",
        "upstream listing timeout",
    ));
    let pipeline = task14_ingestion_pipeline(pool.clone(), repository.clone(), tenant_id, "nango");
    let mut steps =
        DbKnowledgeAutoSyncSteps::new(repository.clone(), provider.clone(), pipeline, 2, "task14");

    let error = run_knowledge_sync_ingestion_workflow(
        &mut steps,
        KnowledgeSyncIngestionRequest { sync_run_uid },
    )
    .await
    .expect_err("provider listing failure should stop the workflow");

    assert!(
        handler_error_text(&error).contains("upstream listing timeout"),
        "workflow error should preserve the safe provider failure message"
    );
    assert_eq!(
        provider.list_changed_record_requests(),
        vec![FakeListChangedRecordsRequest {
            connection_uid,
            cursor: None,
            limit: Some(2),
            modified_after: Some(modified_after),
            variant: None,
        }]
    );
    let run = repository
        .get_sync_run(sync_run_uid)
        .await
        .expect("read failed sync run")
        .expect("failed sync run should exist");
    assert_eq!(run.status, SyncRunStatus::FailedRetryable);
    assert_eq!(run.error_code.as_deref(), Some("provider_error_retryable"));
    assert_eq!(run.records_seen, 0);
    assert_eq!(run.records_changed, 0);
    assert_eq!(run.records_ingested, 0);
    assert_eq!(run.records_failed, 1);
    assert!(run.finished_at.is_some());
    assert!(
        repository
            .list_objects(tenant_id, Some(connection_uid), None, 10)
            .await
            .expect("list objects after failed sync")
            .is_empty()
    );
}

#[tokio::test]
async fn knowledge_sync_ingestion_workflow_paginates_caps_and_completes() {
    // Pins: the workflow lists provider pages with cursor/limit state, applies only the capped records, and completes the run.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let sync_run_uid = Uuid::now_v7();
    let modified_after = moa_test_support::fixtures::pg_now();
    let mut steps = FakeKnowledgeSyncIngestionSteps::new(KnowledgeSyncPreparedRun {
        run: KnowledgeSyncRun {
            sync_run_uid,
            tenant_id,
            connection_uid,
            parser: Some("native".to_string()),
            max_records: Some(3),
            information_barrier: None,
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
            started_at: moa_test_support::fixtures::pg_now(),
            finished_at: None,
            provider_trigger_completed_at: None,
        },
        connection: KnowledgeConnection {
            acl_mode: moa_knowledge::domain::ConnectionAclMode::TenantPublic,
            connection_uid,
            tenant_id,
            provider: PROVIDER.to_string(),
            connector: CONNECTOR.to_string(),
            provider_account_id: "provider-account-1".to_string(),
            credential_ref: "resolved-provider-token".to_string(),
            status: ConnectionStatus::Active,
            metadata: json!({}),
            source_selection: json!({}),
            information_barrier: None,
            created_at: moa_test_support::fixtures::pg_now(),
            updated_at: moa_test_support::fixtures::pg_now(),
            last_synced_at: Some(modified_after),
        },
        provider: PROVIDER.to_string(),
        parser_label: "native".to_string(),
        page_size: 2,
        max_records: 3,
    })
    .with_pages(vec![
        fake_record_page(&["doc-1", "doc-2"], Some("page-2")),
        fake_record_page(&["doc-3", "doc-4"], Some("page-3")),
    ]);

    let report = run_knowledge_sync_ingestion_workflow(
        &mut steps,
        KnowledgeSyncIngestionRequest { sync_run_uid },
    )
    .await
    .expect("workflow should complete capped pagination");

    assert_eq!(report.status, "completed");
    assert_eq!(report.records_listed, 3);
    assert_eq!(report.records_applied, 3);
    assert_eq!(report.records_pruned, 0);
    assert_eq!(
        steps.status_transitions,
        vec![
            SyncRunStatus::ProviderSynced,
            SyncRunStatus::Ingesting,
            SyncRunStatus::Completed
        ]
    );
    assert_eq!(
        steps.list_calls,
        vec![
            FakeListPageCall {
                cursor: None,
                limit: 2,
                page_index: 0,
                credential_ref: "resolved-provider-token".to_string(),
                modified_after: Some(modified_after),
            },
            FakeListPageCall {
                cursor: Some("page-2".to_string()),
                limit: 1,
                page_index: 1,
                credential_ref: "resolved-provider-token".to_string(),
                modified_after: Some(modified_after),
            },
        ]
    );
    assert_eq!(
        steps.apply_calls,
        vec![
            FakeApplyPageCall {
                page_index: 0,
                source_ids: vec!["doc-1".to_string(), "doc-2".to_string()],
            },
            FakeApplyPageCall {
                page_index: 1,
                source_ids: vec!["doc-3".to_string()],
            },
        ]
    );
    assert!(steps.fail_calls.is_empty());
    assert!(steps.prune_calls.is_empty());
}

#[tokio::test]
async fn knowledge_sync_ingestion_workflow_empty_page_completes_with_zero_counters() {
    // Pins: an empty provider page still runs the page application boundary and completes with zero records.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let sync_run_uid = Uuid::now_v7();
    let mut steps = FakeKnowledgeSyncIngestionSteps::new(fake_prepared_sync_run(
        tenant_id,
        connection_uid,
        sync_run_uid,
        10,
    ))
    .with_pages(vec![fake_record_page(&[], None)]);

    let report = run_knowledge_sync_ingestion_workflow(
        &mut steps,
        KnowledgeSyncIngestionRequest { sync_run_uid },
    )
    .await
    .expect("empty provider page should complete");

    assert_eq!(report.records_listed, 0);
    assert_eq!(report.records_applied, 0);
    assert_eq!(report.records_pruned, 0);
    assert_eq!(steps.list_calls.len(), 1);
    assert_eq!(steps.apply_calls.len(), 1);
    assert_eq!(steps.apply_calls[0].source_ids, Vec::<String>::new());
    assert_eq!(
        steps.prune_calls,
        vec![FakePruneCall {
            source_ids: Vec::new()
        }]
    );
    assert_eq!(
        steps.status_transitions,
        vec![
            SyncRunStatus::ProviderSynced,
            SyncRunStatus::Ingesting,
            SyncRunStatus::Completed
        ]
    );
}

#[tokio::test]
async fn knowledge_sync_ingestion_workflow_prunes_after_full_selection_refresh() {
    // Pins: a full selected-source refresh carries all seen source IDs into one durable prune step.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let sync_run_uid = Uuid::now_v7();
    let mut steps = FakeKnowledgeSyncIngestionSteps::new(fake_prepared_sync_run(
        tenant_id,
        connection_uid,
        sync_run_uid,
        10,
    ))
    .with_pages(vec![fake_record_page(&["doc-b", "doc-a"], None)]);

    let report = run_knowledge_sync_ingestion_workflow(
        &mut steps,
        KnowledgeSyncIngestionRequest { sync_run_uid },
    )
    .await
    .expect("full source selection refresh should complete and prune unseen objects");

    assert_eq!(report.status, "completed");
    assert_eq!(report.records_listed, 2);
    assert_eq!(report.records_applied, 2);
    assert_eq!(
        steps.prune_calls,
        vec![FakePruneCall {
            source_ids: vec!["doc-a".to_string(), "doc-b".to_string()]
        }]
    );
    assert!(steps.fail_calls.is_empty());
}

#[tokio::test]
async fn knowledge_sync_ingestion_workflow_derives_run_identity_and_pages_journal_boundaries() {
    // Pins: the report derives tenant/connection from the stored sync run, and the workflow
    // threads cursors across one list+apply journal step per provider page.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let sync_run_uid = Uuid::now_v7();
    let last_synced_at = moa_test_support::fixtures::pg_now();
    let mut prepared = fake_prepared_sync_run(tenant_id, connection_uid, sync_run_uid, 10);
    // A prior watermark keeps this an incremental sync so the listing step receives
    // `modified_after` and the exhaustive-prune branch stays inactive.
    prepared.connection.last_synced_at = Some(last_synced_at);
    let mut steps = FakeKnowledgeSyncIngestionSteps::new(prepared).with_pages(vec![
        fake_record_page(&["doc-1", "doc-2"], Some("page-2")),
        fake_record_page(&["doc-3"], None),
    ]);

    let report = run_knowledge_sync_ingestion_workflow(
        &mut steps,
        KnowledgeSyncIngestionRequest { sync_run_uid },
    )
    .await
    .expect("incremental ingestion across two pages should complete");

    // The report identity is derived from the stored run, not from the request alone.
    assert_eq!(report.sync_run_uid, sync_run_uid);
    assert_eq!(report.tenant_id, tenant_id);
    assert_eq!(report.connection_uid, connection_uid);
    assert_eq!(report.status, "completed");
    assert_eq!(report.records_listed, 3);
    assert_eq!(report.records_applied, 3);
    assert_eq!(report.records_pruned, 0);

    // The run transitions ProviderSynced -> Ingesting (prepare) -> Completed (complete).
    assert_eq!(
        steps.status_transitions,
        vec![
            SyncRunStatus::ProviderSynced,
            SyncRunStatus::Ingesting,
            SyncRunStatus::Completed
        ]
    );

    // One listing journal step per page, threading the provider cursor and watermark forward.
    assert_eq!(
        steps.list_calls,
        vec![
            FakeListPageCall {
                cursor: None,
                limit: 10,
                page_index: 0,
                credential_ref: "resolved-provider-token".to_string(),
                modified_after: Some(last_synced_at),
            },
            FakeListPageCall {
                cursor: Some("page-2".to_string()),
                limit: 8,
                page_index: 1,
                credential_ref: "resolved-provider-token".to_string(),
                modified_after: Some(last_synced_at),
            },
        ]
    );

    // One application journal step per page, page-indexed alongside the listing steps.
    assert_eq!(
        steps.apply_calls,
        vec![
            FakeApplyPageCall {
                page_index: 0,
                source_ids: vec!["doc-1".to_string(), "doc-2".to_string()],
            },
            FakeApplyPageCall {
                page_index: 1,
                source_ids: vec!["doc-3".to_string()],
            },
        ]
    );

    // An incremental sync (watermark present) never prunes and never marks the run failed.
    assert!(steps.prune_calls.is_empty());
    assert!(steps.fail_calls.is_empty());
}

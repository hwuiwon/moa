//! Graph occurrence identity scenarios: equal content, distinct occurrences.

use super::*;

/// Chunking that turns every paragraph into its own chunk, so occurrence
/// behaviour is visible per paragraph.
fn per_paragraph_chunking() -> ChunkingConfig {
    ChunkingConfig {
        target_tokens: 1,
        max_tokens: 16,
        min_tokens: 1,
    }
}

/// Builds a pipeline over one isolated database with the shared test doubles.
async fn occurrence_pipeline(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    connection_uid: Uuid,
) -> (
    Arc<PostgresKnowledgeRepository>,
    Arc<CountingEmbedder>,
    Arc<FakeGraphWriter>,
    KnowledgeIngestionPipeline<
        PostgresKnowledgeRepository,
        ParagraphParser,
        CountingEmbedder,
        FakeGraphWriter,
    >,
) {
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        RlsContext::tenant(tenant_id),
    ));
    let embedder = Arc::new(CountingEmbedder::default());
    let graph = Arc::new(FakeGraphWriter::default());
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        Arc::new(ParagraphParser),
        embedder.clone(),
        graph.clone(),
        KnowledgeIngestionPipelineConfig {
            chunking: per_paragraph_chunking(),
            provider: "test_provider".to_string(),
            parser_label: "test_parser".to_string(),
        },
    );
    repository
        .upsert_connection(drive_connection(connection_uid, tenant_id))
        .await
        .expect("upsert connection");
    (repository, embedder, graph, pipeline)
}

#[tokio::test]
async fn equal_text_in_two_documents_gets_distinct_occurrences_db_memory() {
    // Pins: two documents containing byte-identical text produce two chunk
    // occurrences with distinct `chunk_uid == graph_node_uid`, their own vectors,
    // and their own containment edge to their own document version — and deleting
    // one document invalidates only its own occurrence, leaving the other
    // retrievable against its own source and version.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let (repository, _embedder, graph, pipeline) =
        occurrence_pipeline(&pool, tenant_id, connection_uid).await;

    const SHARED_TEXT: &str = "Reimbursement requires manager approval.";
    let ingest_run = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            ingest_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![
                    record_with_source("doc-alpha", "v1", false, SHARED_TEXT),
                    record_with_source("doc-beta", "v1", false, SHARED_TEXT),
                ],
                next_cursor: None,
            },
        )
        .await
        .expect("ingest two documents with identical text");

    let alpha_uid = object_uid_for_source(connection_uid, "doc-alpha");
    let beta_uid = object_uid_for_source(connection_uid, "doc-beta");
    let alpha = occurrence_rows(&pool, alpha_uid).await;
    let beta = occurrence_rows(&pool, beta_uid).await;
    assert_eq!(alpha.len(), 1, "{alpha:?}");
    assert_eq!(beta.len(), 1, "{beta:?}");
    assert_eq!(
        alpha[0].chunk_hash, beta[0].chunk_hash,
        "the two documents must really carry identical content"
    );
    assert_ne!(
        alpha[0].chunk_uid, beta[0].chunk_uid,
        "identical content in two documents must not share one occurrence identity"
    );
    assert_eq!(alpha[0].graph_node_uid, alpha[0].chunk_uid);
    assert_eq!(beta[0].graph_node_uid, beta[0].chunk_uid);
    assert_ne!(alpha[0].version_uid, beta[0].version_uid);

    // Each occurrence owns a graph node, a vector, and a containment edge from
    // its own document version.
    assert!(graph.has_node(alpha[0].chunk_uid));
    assert!(graph.has_node(beta[0].chunk_uid));
    assert!(graph.has_vector(alpha[0].chunk_uid));
    assert!(graph.has_vector(beta[0].chunk_uid));
    assert_eq!(graph.vector_count(), 2);
    assert_eq!(
        graph.edge_sources_into(&format!("chunk:{}", alpha[0].chunk_uid), "HAS_CHUNK"),
        vec![format!("document:{}", alpha[0].version_uid)],
        "alpha's occurrence must be contained by alpha's document version only"
    );
    assert_eq!(
        graph.edge_sources_into(&format!("chunk:{}", beta[0].chunk_uid), "HAS_CHUNK"),
        vec![format!("document:{}", beta[0].version_uid)],
        "beta's occurrence must be contained by beta's document version only"
    );
    assert_eq!(
        graph
            .node_properties(alpha[0].chunk_uid)
            .expect("alpha occurrence node")["version_uid"],
        json!(alpha[0].version_uid),
        "an occurrence node names the document version it belongs to"
    );

    // Deleting alpha at the provider must not touch beta's occurrence.
    let delete_run = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            delete_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record_with_source("doc-alpha", "v2", true, "")],
                next_cursor: None,
            },
        )
        .await
        .expect("handle alpha deletion");

    assert_eq!(
        graph.invalidated_uids(),
        vec![alpha[0].chunk_uid],
        "only alpha's occurrence may be invalidated"
    );
    assert!(!graph.has_vector(alpha[0].chunk_uid));
    assert!(
        graph.has_vector(beta[0].chunk_uid),
        "beta's vector must survive alpha's deletion"
    );
    assert_eq!(object_status(&pool, alpha_uid).await, "deleted");
    assert_eq!(object_status(&pool, beta_uid).await, "active");
    assert_eq!(tombstoned_chunk_count(&pool, alpha_uid).await, 1);
    assert_eq!(tombstoned_chunk_count(&pool, beta_uid).await, 0);
    let beta_after = occurrence_rows(&pool, beta_uid).await;
    assert_eq!(
        beta_after, beta,
        "beta's occurrence, version, and content are untouched"
    );
}

#[tokio::test]
async fn repeated_paragraph_reuses_computation_but_not_the_association_db_memory() {
    // Pins: two occurrences whose COMPLETE contextual embedding input (document
    // title, heading path, chunk text) is identical pay for one embedding
    // computation, yet each occurrence still gets its own persisted embedding
    // association keyed by its own `chunk_uid`.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let (repository, embedder, graph, pipeline) =
        occurrence_pipeline(&pool, tenant_id, connection_uid).await;

    let sync_run_uid = create_run(&repository, tenant_id, connection_uid).await;
    let report = pipeline
        .ingest_record_page(
            sync_run_uid,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record(
                    "v1",
                    false,
                    "Approval is required.\n\nApproval is required.",
                )],
                next_cursor: None,
            },
        )
        .await
        .expect("ingest a document that repeats one paragraph");

    let object_uid = object_uid(connection_uid);
    let occurrences = occurrence_rows(&pool, object_uid).await;
    assert_eq!(occurrences.len(), 2, "{occurrences:?}");
    assert_eq!(
        occurrences[0].chunk_hash, occurrences[1].chunk_hash,
        "the repeated paragraph must really produce equal content hashes"
    );
    assert_ne!(occurrences[0].chunk_uid, occurrences[1].chunk_uid);
    assert_eq!(occurrences[0].graph_node_uid, occurrences[0].chunk_uid);
    assert_eq!(occurrences[1].graph_node_uid, occurrences[1].chunk_uid);

    assert_eq!(
        embedder.embedded_count(),
        1,
        "equal contextual input is embedded once"
    );
    assert!(graph.has_vector(occurrences[0].chunk_uid));
    assert!(graph.has_vector(occurrences[1].chunk_uid));
    assert_eq!(
        graph.vector_count(),
        2,
        "each occurrence owns its own vector association"
    );
    assert_eq!(report.embeddings_created, 2);
    let embedded = ingestion_step_counters(&pool, sync_run_uid, object_uid, "embedded").await;
    assert_eq!(embedded["embeddings_created"], json!(2));
    assert_eq!(embedded["embeddings_reused"], json!(1));
    assert_eq!(embedded["chunks_embedded"], json!(2));
}

#[tokio::test]
async fn new_version_reoccurs_unchanged_text_and_invalidates_the_superseded_one_db_memory() {
    // Pins: a new document version creates new occurrences for EVERY chunk,
    // including text carried over unchanged, and invalidates the superseded
    // occurrences instead of leaving two live occurrences of the same paragraph.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let (repository, _embedder, graph, pipeline) =
        occurrence_pipeline(&pool, tenant_id, connection_uid).await;

    let first_run = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            first_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("v1", false, "Policy is stable.\n\nBudget is 10.")],
                next_cursor: None,
            },
        )
        .await
        .expect("ingest first version");
    let object_uid = object_uid(connection_uid);
    let first = occurrence_rows(&pool, object_uid).await;
    assert_eq!(first.len(), 2, "{first:?}");

    let second_run = create_run(&repository, tenant_id, connection_uid).await;
    let edited = pipeline
        .ingest_record_page(
            second_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("v2", false, "Policy is stable.\n\nBudget is 20.")],
                next_cursor: None,
            },
        )
        .await
        .expect("ingest an edit that keeps one paragraph unchanged");

    let all = occurrence_rows(&pool, object_uid).await;
    assert_eq!(all.len(), 4, "{all:?}");
    let (superseded, current): (Vec<_>, Vec<_>) = all
        .iter()
        .partition(|row| row.version_uid == first[0].version_uid);
    assert_eq!(superseded.len(), 2);
    assert_eq!(current.len(), 2);
    assert!(
        superseded.iter().all(|row| !row.active),
        "every occurrence of the superseded version is tombstoned: {superseded:?}"
    );
    assert!(
        current.iter().all(|row| row.active),
        "every occurrence of the new version is active: {current:?}"
    );

    // The unchanged paragraph is the load-bearing case: same content hash, new
    // occurrence, new vector, and the old occurrence invalidated.
    let carried_over_old = superseded
        .iter()
        .find(|row| row.ordinal == 0)
        .expect("superseded first paragraph");
    let carried_over_new = current
        .iter()
        .find(|row| row.ordinal == 0)
        .expect("current first paragraph");
    assert_eq!(carried_over_old.chunk_hash, carried_over_new.chunk_hash);
    assert_ne!(carried_over_old.chunk_uid, carried_over_new.chunk_uid);
    assert!(graph.has_vector(carried_over_new.chunk_uid));
    assert!(!graph.has_vector(carried_over_old.chunk_uid));

    let mut invalidated = graph.invalidated_uids();
    invalidated.sort();
    let mut expected = superseded
        .iter()
        .map(|row| row.chunk_uid)
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(
        invalidated, expected,
        "exactly the superseded occurrences are invalidated"
    );
    assert_eq!(graph.vector_count(), 2);
    assert_eq!(
        edited.embeddings_created, 2,
        "both new occurrences get an association, unchanged content included"
    );
    assert_eq!(
        chunks_with_occurrence_identity(&pool, object_uid).await,
        4,
        "every stored chunk keeps occurrence identity as its graph identity"
    );
}

#[tokio::test]
async fn object_deletion_invalidates_occurrences_of_every_active_version_db_memory() {
    // Pins: whole-object deletion covers every active version's occurrences, not
    // just the latest version's. A version transition whose invalidation failed
    // leaves older occurrences active and retrievable; the deletion that follows
    // must clear those too.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let (repository, _embedder, graph, pipeline) =
        occurrence_pipeline(&pool, tenant_id, connection_uid).await;

    let first_run = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            first_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("v1", false, "Alpha one.\n\nBeta one.")],
                next_cursor: None,
            },
        )
        .await
        .expect("ingest first version");
    let object_uid = object_uid(connection_uid);

    // The edit persists its own occurrences and then fails invalidation, so both
    // versions stay active — the state a latest-version-only deletion strands.
    graph.set_fail_invalidate(true);
    let edit_run = create_run(&repository, tenant_id, connection_uid).await;
    let failed = pipeline
        .ingest_record_page(
            edit_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("v2", false, "Alpha two.\n\nBeta two.")],
                next_cursor: None,
            },
        )
        .await;
    assert!(failed.is_err(), "invalidation failure must surface");
    assert_eq!(active_chunk_count(&pool, object_uid).await, 4);

    graph.set_fail_invalidate(false);
    let delete_run = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
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

    let all = occurrence_rows(&pool, object_uid).await;
    assert_eq!(all.len(), 4, "{all:?}");
    assert!(
        all.iter().all(|row| !row.active),
        "no occurrence of any version stays active after deletion: {all:?}"
    );
    let mut invalidated = graph.invalidated_uids();
    invalidated.sort();
    let mut expected = all.iter().map(|row| row.chunk_uid).collect::<Vec<_>>();
    expected.sort();
    assert_eq!(
        invalidated, expected,
        "deletion invalidates every version's occurrences"
    );
    assert_eq!(graph.vector_count(), 0);
    assert_eq!(object_status(&pool, object_uid).await, "deleted");
}

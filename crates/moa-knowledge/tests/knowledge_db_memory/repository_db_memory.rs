//! Postgres repository coverage for tenant knowledge-base RLS and timelines.

use chrono::{Duration, Utc};
use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::RlsContext;
use moa_knowledge::{
    domain::{
        ConnectionStatus, DocumentVersion, IngestionStepStatus, KnowledgeBlock, KnowledgeChunk,
        KnowledgeConnection, KnowledgeIngestionStep, KnowledgeObject, KnowledgeSyncRun,
        ObjectStatus, SyncRunStatus,
    },
    repository::{
        KnowledgeDiscoveryStore, KnowledgeRepository, PostgresKnowledgeDiscoveryStore,
        PostgresKnowledgeRepository, ProviderAccountConnectionLookup,
    },
};
use moa_test_support::postgres;
use serde_json::{Value, json};
use uuid::Uuid;

fn repository(db: &postgres::TestDb, tenant_id: TenantId) -> PostgresKnowledgeRepository {
    PostgresKnowledgeRepository::scoped_for_app_role(
        db.store().pool().clone(),
        RlsContext::tenant(tenant_id),
    )
}

fn discovery(db: &postgres::TestDb) -> PostgresKnowledgeDiscoveryStore {
    PostgresKnowledgeDiscoveryStore::for_app_role(db.store().pool().clone())
}

fn connection(tenant_id: TenantId, label: &str) -> KnowledgeConnection {
    let now = Utc::now();
    KnowledgeConnection {
        connection_uid: Uuid::now_v7(),
        tenant_id,
        provider: "nango".to_string(),
        connector: format!("google-drive-{label}"),
        provider_account_id: format!("provider-account-{label}"),
        credential_ref: format!("vault://tenant/{label}/knowledge"),
        status: ConnectionStatus::Active,
        metadata: json!({ "safe_label": label }),
        source_selection: json!({}),
        information_barrier: None,
        created_at: now,
        updated_at: now,
        last_synced_at: None,
    }
}

fn sync_run(tenant_id: TenantId, connection_uid: Uuid) -> KnowledgeSyncRun {
    KnowledgeSyncRun {
        sync_run_uid: Uuid::now_v7(),
        tenant_id,
        connection_uid,
        parser: Some("native".to_string()),
        max_records: None,
        information_barrier: None,
        status: SyncRunStatus::Ingesting,
        records_seen: 2,
        records_changed: 0,
        records_deleted: 0,
        records_ingested: 1,
        records_failed: 0,
        objects_parsed: 0,
        chunks_embedded: 0,
        graph_nodes_upserted: 0,
        graph_edges_upserted: 0,
        error_code: None,
        started_at: moa_test_support::fixtures::pg_now(),
        finished_at: None,
        provider_trigger_completed_at: None,
    }
}

fn object(tenant_id: TenantId, connection_uid: Uuid, label: &str) -> KnowledgeObject {
    KnowledgeObject {
        object_uid: Uuid::now_v7(),
        tenant_id,
        connection_uid,
        object_type: "document".to_string(),
        source_id: format!("external-object-{label}"),
        parent_source_id: None,
        source_uri: Some(format!("https://example.test/{label}")),
        title: Some(format!("Knowledge {label}")),
        change_token: Some(format!("etag-{label}")),
        metadata: json!({ "safe_label": label }),
        status: ObjectStatus::Active,
        source_updated_at: Some(moa_test_support::fixtures::pg_now()),
        deleted_at: None,
    }
}

fn step(
    sync_run_uid: Uuid,
    object_uid: Option<Uuid>,
    stage: &str,
    status: IngestionStepStatus,
    offset_ms: i64,
) -> KnowledgeIngestionStep {
    let started_at = Utc::now() + Duration::milliseconds(offset_ms);
    KnowledgeIngestionStep {
        step_uid: Uuid::now_v7(),
        sync_run_uid,
        object_uid,
        step: stage.to_string(),
        status,
        started_at,
        ended_at: Some(started_at + Duration::milliseconds(7)),
        duration_ms: Some(7),
        counters: json!({ "records_seen": 1 }),
        summary: Some(format!("{stage} completed without credential details")),
        retry_count: 0,
        error_code: None,
    }
}

#[tokio::test]
async fn discovery_rejects_missing_and_ambiguous_provider_bindings_db_knowledge() {
    // Pins: control-plane discovery fails closed unless provider-owned account identity is unique.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated knowledge DB");
    let tenant_a = TenantId::from(Uuid::now_v7());
    let tenant_b = TenantId::from(Uuid::now_v7());
    let repo_a = repository(&db, tenant_a);
    let repo_b = repository(&db, tenant_b);
    let mut connection_a = connection(tenant_a, "tenant-a");
    let mut connection_b = connection(tenant_b, "tenant-b");
    connection_a.connector = "shared-connector".to_string();
    connection_b.connector = "shared-connector".to_string();
    connection_a.provider_account_id = "shared-account".to_string();
    connection_b.provider_account_id = "shared-account".to_string();
    repo_a
        .upsert_connection(connection_a)
        .await
        .expect("insert tenant A connection");
    repo_b
        .upsert_connection(connection_b)
        .await
        .expect("insert tenant B connection");

    let discovery = discovery(&db);
    assert_eq!(
        discovery
            .lookup_connection_by_provider_account(
                "nango",
                Some("shared-connector"),
                "missing-account",
            )
            .await
            .expect("missing provider lookup should complete"),
        ProviderAccountConnectionLookup::NotFound
    );
    assert_eq!(
        discovery
            .lookup_connection_by_provider_account(
                "nango",
                Some("shared-connector"),
                "shared-account",
            )
            .await
            .expect("ambiguous provider lookup should complete"),
        ProviderAccountConnectionLookup::Ambiguous { matches: 2 }
    );
}

#[tokio::test]
async fn scoped_repository_hides_other_tenant_rows_and_returns_redacted_timelines_db_knowledge() {
    // Pins: knowledge repository operations use ScopedConn tenant RLS and expose safe step timelines.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated knowledge DB");
    let tenant_a = TenantId::from(Uuid::now_v7());
    let tenant_b = TenantId::from(Uuid::now_v7());
    let repo_a = repository(&db, tenant_a);
    let repo_b = repository(&db, tenant_b);

    let mut connection_a = connection(tenant_a, "tenant-a");
    connection_a.last_synced_at = Some(moa_test_support::fixtures::pg_now());
    let connection_b = connection(tenant_b, "tenant-b");
    repo_a
        .upsert_connection(connection_a.clone())
        .await
        .expect("insert tenant A connection");
    repo_b
        .upsert_connection(connection_b.clone())
        .await
        .expect("insert tenant B connection");
    let selected_sources = json!({
        "metadata": {
            "selected_folder_ids": ["folder-a"],
            "access_token": "must-redact"
        },
        "variant": "selected-sources"
    });
    let updated_connection = repo_a
        .update_connection_source_selection(connection_a.connection_uid, selected_sources)
        .await
        .expect("update tenant A source selection");
    assert_eq!(
        updated_connection.source_selection["metadata"]["selected_folder_ids"],
        json!(["folder-a"])
    );
    assert!(
        updated_connection.source_selection["metadata"]
            .get("access_token")
            .is_none()
    );
    assert_eq!(updated_connection.last_synced_at, None);
    assert!(
        repo_a
            .update_connection_source_selection(connection_b.connection_uid, json!({}))
            .await
            .is_err(),
        "tenant A must not update tenant B source selection"
    );
    let connection_summaries = repo_a
        .list_connections(tenant_a, Some("nango"))
        .await
        .expect("list tenant A connections");
    assert_eq!(connection_summaries.len(), 1);
    assert_eq!(
        connection_summaries[0].connection.source_selection["variant"],
        "selected-sources"
    );
    assert_eq!(connection_summaries[0].connection.last_synced_at, None);

    let run_a = sync_run(tenant_a, connection_a.connection_uid);
    let run_b = sync_run(tenant_b, connection_b.connection_uid);
    repo_a
        .create_sync_run(run_a.clone())
        .await
        .expect("insert tenant A sync run");
    repo_b
        .create_sync_run(run_b.clone())
        .await
        .expect("insert tenant B sync run");

    let object_a = object(tenant_a, connection_a.connection_uid, "tenant-a");
    let object_b = object(tenant_b, connection_b.connection_uid, "tenant-b");
    repo_a
        .upsert_object(object_a.clone())
        .await
        .expect("insert tenant A object");
    repo_b
        .upsert_object(object_b.clone())
        .await
        .expect("insert tenant B object");

    for step in [
        step(
            run_a.sync_run_uid,
            None,
            "provider_records_listed",
            IngestionStepStatus::Completed,
            0,
        ),
        step(
            run_a.sync_run_uid,
            Some(object_a.object_uid),
            "parse_completed",
            IngestionStepStatus::Completed,
            10,
        ),
        step(
            run_a.sync_run_uid,
            Some(object_a.object_uid),
            "graph_upserted",
            IngestionStepStatus::Completed,
            20,
        ),
    ] {
        repo_a
            .record_ingestion_step(step)
            .await
            .expect("record tenant A ingestion step");
    }
    repo_b
        .record_ingestion_step(step(
            run_b.sync_run_uid,
            Some(object_b.object_uid),
            "parse_completed",
            IngestionStepStatus::Completed,
            0,
        ))
        .await
        .expect("record tenant B ingestion step");

    assert_eq!(
        repo_a
            .get_connection(connection_b.connection_uid)
            .await
            .expect("tenant A lookup for tenant B connection"),
        None
    );
    assert_eq!(
        repo_a
            .get_object(object_b.object_uid)
            .await
            .expect("tenant A lookup for tenant B object"),
        None
    );
    assert!(
        repo_a
            .sync_run_timeline(run_b.sync_run_uid)
            .await
            .expect("tenant A timeline lookup for tenant B run")
            .is_empty()
    );

    let run_timeline = repo_a
        .sync_run_timeline(run_a.sync_run_uid)
        .await
        .expect("load tenant A sync-run timeline");
    assert_eq!(
        run_timeline
            .iter()
            .map(|entry| entry.step.as_str())
            .collect::<Vec<_>>(),
        vec![
            "provider_records_listed",
            "parse_completed",
            "graph_upserted"
        ]
    );
    assert_eq!(
        run_timeline
            .iter()
            .map(|entry| entry.summary.as_deref())
            .collect::<Vec<_>>(),
        vec![
            Some("provider_records_listed completed without credential details"),
            Some("parse_completed completed without credential details"),
            Some("graph_upserted completed without credential details"),
        ]
    );

    let object_timeline = repo_a
        .object_timeline(object_a.object_uid)
        .await
        .expect("load tenant A object timeline");
    assert_eq!(
        object_timeline
            .iter()
            .map(|entry| entry.step.as_str())
            .collect::<Vec<_>>(),
        vec!["parse_completed", "graph_upserted"]
    );
}

#[tokio::test]
async fn disable_connection_updates_only_requested_tenant_connection() {
    // Pins: disconnect flows can disable one tenant knowledge connection without crossing RLS tenants.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated knowledge DB");
    let tenant_a = TenantId::from(Uuid::now_v7());
    let tenant_b = TenantId::from(Uuid::now_v7());
    let repo_a = repository(&db, tenant_a);
    let repo_b = repository(&db, tenant_b);

    let connection_a = connection(tenant_a, "disable-a");
    let connection_b = connection(tenant_b, "disable-b");
    repo_a
        .upsert_connection(connection_a.clone())
        .await
        .expect("insert tenant A connection");
    repo_b
        .upsert_connection(connection_b.clone())
        .await
        .expect("insert tenant B connection");

    let disabled = repo_a
        .disable_connection(tenant_a, connection_a.connection_uid)
        .await
        .expect("tenant A should disable its own connection");
    assert_eq!(disabled.connection_uid, connection_a.connection_uid);
    assert_eq!(disabled.status, ConnectionStatus::Disabled);

    let tenant_a_connections = repo_a
        .list_connections(tenant_a, Some("nango"))
        .await
        .expect("tenant A should list its own disabled connection");
    assert_eq!(tenant_a_connections.len(), 1);
    assert_eq!(
        tenant_a_connections[0].connection.status,
        ConnectionStatus::Disabled
    );

    let tenant_b_connections = repo_b
        .list_connections(tenant_b, Some("nango"))
        .await
        .expect("tenant B should list its unchanged connection");
    assert_eq!(tenant_b_connections.len(), 1);
    assert_eq!(
        tenant_b_connections[0].connection.status,
        ConnectionStatus::Active
    );

    assert!(
        repo_a
            .disable_connection(tenant_a, connection_b.connection_uid)
            .await
            .is_err(),
        "tenant A must not disable tenant B's connection"
    );
}

fn document_version(object_uid: Uuid, label: &str) -> DocumentVersion {
    DocumentVersion {
        version_uid: Uuid::now_v7(),
        object_uid,
        parser: "native".to_string(),
        parser_job_id: None,
        content_hash: format!("content-hash-{label}"),
        metadata: json!({ "safe_label": label }),
        created_at: moa_test_support::fixtures::pg_now(),
    }
}

#[allow(clippy::too_many_arguments)]
fn block(
    version_uid: Uuid,
    ordinal: u32,
    element_id: &str,
    block_hash: &str,
    text: &str,
    heading_path: Vec<String>,
    metadata: Value,
) -> KnowledgeBlock {
    KnowledgeBlock {
        block_uid: Uuid::now_v7(),
        version_uid,
        element_id: element_id.to_string(),
        block_hash: block_hash.to_string(),
        normalized_text: text.to_string(),
        heading_path,
        ordinal,
        metadata,
    }
}

#[allow(clippy::too_many_arguments)]
fn chunk(
    version_uid: Uuid,
    ordinal: u32,
    chunk_hash: &str,
    block_hashes: Vec<String>,
    text: &str,
    heading_path: Vec<String>,
    metadata: Value,
) -> KnowledgeChunk {
    KnowledgeChunk {
        chunk_uid: Uuid::now_v7(),
        version_uid,
        chunk_hash: chunk_hash.to_string(),
        block_hashes,
        text: text.to_string(),
        heading_path,
        ordinal,
        token_count: ordinal as usize + 1,
        metadata,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct StoredBlock {
    element_id: String,
    block_hash: String,
    ordinal: i32,
    normalized_text: String,
    heading_path: Vec<String>,
    metadata: Value,
}

async fn stored_blocks(pool: &sqlx::PgPool, version_uid: Uuid) -> Vec<StoredBlock> {
    sqlx::query_as::<_, (String, String, i32, String, Vec<String>, Value)>(
        r#"
        SELECT element_id, block_hash, ordinal, normalized_text, heading_path, metadata
        FROM moa.knowledge_blocks
        WHERE document_version_id = $1
        ORDER BY ordinal ASC
        "#,
    )
    .bind(version_uid)
    .fetch_all(pool)
    .await
    .expect("load stored blocks")
    .into_iter()
    .map(
        |(element_id, block_hash, ordinal, normalized_text, heading_path, metadata)| StoredBlock {
            element_id,
            block_hash,
            ordinal,
            normalized_text,
            heading_path,
            metadata,
        },
    )
    .collect()
}

#[tokio::test]
async fn replace_blocks_and_chunks_batch_round_trip_persists_all_rows_db_knowledge() {
    // Pins: the multi-row UNNEST batch insert persists every block and chunk row
    // with the same content the former per-row loop wrote, stores each chunk's
    // graph occurrence identity as its own `chunk_uid`, preserves empty and
    // populated TEXT[] arrays and JSON metadata, and still fully replaces prior
    // rows.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated knowledge DB");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let repo = repository(&db, tenant_id);

    let connection = connection(tenant_id, "batch");
    repo.upsert_connection(connection.clone())
        .await
        .expect("insert connection");
    let object = object(tenant_id, connection.connection_uid, "batch");
    repo.upsert_object(object.clone())
        .await
        .expect("insert object");
    let version = document_version(object.object_uid, "batch");
    repo.insert_document_version(version.clone())
        .await
        .expect("insert document version");

    let blocks = vec![
        block(
            version.version_uid,
            0,
            "elem-0",
            "bh-0",
            "First block.",
            Vec::new(),
            Value::Null,
        ),
        block(
            version.version_uid,
            1,
            "elem-1",
            "bh-1",
            "Second block.",
            vec!["Intro".to_string()],
            json!({ "safe": 1 }),
        ),
        block(
            version.version_uid,
            2,
            "elem-2",
            "bh-2",
            "Third block.",
            vec!["Intro".to_string(), "Detail".to_string()],
            json!({ "safe": 2 }),
        ),
    ];
    repo.replace_blocks(version.version_uid, blocks.clone())
        .await
        .expect("replace blocks");

    let stored = stored_blocks(&pool, version.version_uid).await;
    assert_eq!(stored.len(), 3);
    for (row, expected) in stored.iter().zip(blocks.iter()) {
        assert_eq!(row.element_id, expected.element_id);
        assert_eq!(row.block_hash, expected.block_hash);
        assert_eq!(row.ordinal as u32, expected.ordinal);
        assert_eq!(row.normalized_text, expected.normalized_text);
        assert_eq!(row.heading_path, expected.heading_path);
        assert_eq!(row.metadata, expected.metadata);
    }

    let chunks = vec![
        chunk(
            version.version_uid,
            0,
            "ch-0",
            vec!["bh-0".to_string()],
            "First chunk.",
            Vec::new(),
            json!({ "active": true }),
        ),
        chunk(
            version.version_uid,
            1,
            "ch-1",
            vec!["bh-1a".to_string(), "bh-1b".to_string()],
            "Second chunk.",
            vec!["Intro".to_string()],
            json!({ "active": true, "n": 1 }),
        ),
        chunk(
            version.version_uid,
            2,
            "ch-2",
            Vec::new(),
            "Third chunk.",
            vec!["A".to_string(), "B".to_string()],
            json!({ "active": true }),
        ),
    ];
    repo.replace_chunks(version.version_uid, chunks.clone())
        .await
        .expect("replace chunks");

    let stored_chunks = repo
        .chunks_for_version(version.version_uid)
        .await
        .expect("load chunks");
    assert_eq!(stored_chunks.len(), 3);
    for (row, expected) in stored_chunks.iter().zip(chunks.iter()) {
        assert_eq!(row.chunk_uid, expected.chunk_uid);
        assert_eq!(row.chunk_hash, expected.chunk_hash);
        assert_eq!(row.block_hashes, expected.block_hashes);
        assert_eq!(row.heading_path, expected.heading_path);
        assert_eq!(row.text, expected.text);
        assert_eq!(row.ordinal, expected.ordinal);
        assert_eq!(row.token_count, expected.token_count);
        assert_eq!(row.metadata, expected.metadata);
    }

    // Storage owns the occurrence invariant: the persisted graph identity of every
    // chunk row is that row's own `chunk_uid`.
    let persisted_identities = sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT chunk_uid, graph_node_uid FROM moa.knowledge_chunks \
         WHERE document_version_id = $1 ORDER BY ordinal ASC",
    )
    .bind(version.version_uid)
    .fetch_all(&pool)
    .await
    .expect("load persisted chunk identities");
    assert_eq!(
        persisted_identities,
        chunks
            .iter()
            .map(|chunk| (chunk.chunk_uid, chunk.chunk_uid))
            .collect::<Vec<_>>()
    );

    // Replacing with a smaller set fully clears the prior rows.
    repo.replace_chunks(
        version.version_uid,
        vec![chunk(
            version.version_uid,
            0,
            "ch-only",
            vec!["bh".to_string()],
            "Only chunk.",
            Vec::new(),
            json!({ "active": true }),
        )],
    )
    .await
    .expect("replace chunks again");
    let replaced = repo
        .chunks_for_version(version.version_uid)
        .await
        .expect("reload chunks");
    assert_eq!(replaced.len(), 1);
    assert_eq!(replaced[0].chunk_hash, "ch-only");
}

#[tokio::test]
async fn unseen_active_objects_for_connection_filters_seen_deleted_and_paginates_db_knowledge() {
    // Pins: the SQL prune-set query returns only active, un-seen objects for the
    // tenant/connection ordered by (source_id, object_uid), excludes deleted and
    // seen sources, and honors the keyset cursor plus limit for pagination.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated knowledge DB");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let repo = repository(&db, tenant_id);
    let connection = connection(tenant_id, "unseen");
    repo.upsert_connection(connection.clone())
        .await
        .expect("insert connection");

    let mut object_uids = std::collections::HashMap::new();
    for label in ["doc-a", "doc-b", "doc-c", "doc-d", "doc-e"] {
        let mut obj = object(tenant_id, connection.connection_uid, label);
        obj.source_id = label.to_string();
        repo.upsert_object(obj.clone())
            .await
            .expect("insert object");
        object_uids.insert(label.to_string(), obj.object_uid);
    }
    // doc-c is deleted and must never appear even though it is not "seen".
    repo.mark_object_deleted(object_uids["doc-c"], Utc::now())
        .await
        .expect("delete doc-c");

    let seen = vec!["doc-b".to_string()];
    let all_unseen = repo
        .unseen_active_objects_for_connection(connection.connection_uid, tenant_id, &seen, None, 10)
        .await
        .expect("load unseen objects");
    assert_eq!(
        all_unseen
            .iter()
            .map(|object| object.source_id.as_str())
            .collect::<Vec<_>>(),
        vec!["doc-a", "doc-d", "doc-e"],
    );

    // Keyset pagination: two pages of size two cover the same set without overlap.
    let page_one = repo
        .unseen_active_objects_for_connection(connection.connection_uid, tenant_id, &seen, None, 2)
        .await
        .expect("first page");
    assert_eq!(
        page_one
            .iter()
            .map(|object| object.source_id.as_str())
            .collect::<Vec<_>>(),
        vec!["doc-a", "doc-d"],
    );
    let last = page_one.last().expect("first page is non-empty");
    let cursor = Some((last.source_id.clone(), last.object_uid));
    let page_two = repo
        .unseen_active_objects_for_connection(
            connection.connection_uid,
            tenant_id,
            &seen,
            cursor,
            2,
        )
        .await
        .expect("second page");
    assert_eq!(
        page_two
            .iter()
            .map(|object| object.source_id.as_str())
            .collect::<Vec<_>>(),
        vec!["doc-e"],
    );

    // An empty seen set returns every active object.
    let none_seen = repo
        .unseen_active_objects_for_connection(connection.connection_uid, tenant_id, &[], None, 10)
        .await
        .expect("load with empty seen");
    assert_eq!(
        none_seen
            .iter()
            .map(|object| object.source_id.as_str())
            .collect::<Vec<_>>(),
        vec!["doc-a", "doc-b", "doc-d", "doc-e"],
    );
}

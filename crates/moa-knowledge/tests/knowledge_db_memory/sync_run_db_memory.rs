//! DB integration coverage for tenant knowledge sync-run inspection.

use chrono::Duration;
use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::RlsContext;
use moa_knowledge::{
    domain::{
        ConnectionStatus, DocumentVersion, IngestionStepStatus, KnowledgeConnection,
        KnowledgeIngestionStep, KnowledgeObject, KnowledgeSyncCounters, KnowledgeSyncRun,
        ObjectStatus, SyncRunStatus,
    },
    repository::{
        DocumentVersionIngestionClaim, KnowledgeDiscoveryStore, KnowledgeRepository,
        PostgresKnowledgeDiscoveryStore, PostgresKnowledgeRepository, SyncRunClaim,
    },
};
use moa_test_support::postgres;
use serde_json::json;
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
    let now = moa_test_support::fixtures::pg_now();
    KnowledgeConnection {
        connection_uid: Uuid::now_v7(),
        tenant_id,
        provider: "merge".to_string(),
        connector: format!("crm-{label}"),
        provider_account_id: format!("linked-account-{label}"),
        credential_ref: format!("vault://tenant/{label}/merge"),
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
        records_seen: 1,
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
    }
}

fn object(
    tenant_id: TenantId,
    connection_uid: Uuid,
    label: &str,
    object_type: &str,
) -> KnowledgeObject {
    KnowledgeObject {
        acl: moa_knowledge::domain::ObjectAcl::incomplete(),
        object_uid: Uuid::now_v7(),
        tenant_id,
        connection_uid,
        object_type: object_type.to_string(),
        source_id: format!("source-{label}"),
        parent_source_id: None,
        source_uri: Some(format!("https://source.example/{label}")),
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
    let started_at = moa_test_support::fixtures::pg_now() + Duration::milliseconds(offset_ms);
    KnowledgeIngestionStep {
        step_uid: Uuid::now_v7(),
        sync_run_uid,
        object_uid,
        step: stage.to_string(),
        status,
        started_at,
        ended_at: Some(started_at + Duration::milliseconds(5)),
        duration_ms: Some(5),
        counters: json!({ "records_seen": 1, "stage": stage }),
        summary: Some(format!("{stage} completed")),
        retry_count: 0,
        error_code: None,
    }
}

#[tokio::test]
async fn active_sync_run_claim_allows_one_runner_per_connection_db_knowledge() {
    // Pins: two repository instances racing to start one connection sync produce one active run.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated knowledge DB");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let repo_a = repository(&db, tenant_id);
    let repo_b = repository(&db, tenant_id);

    let connection = connection(tenant_id, "active-claim");
    repo_a
        .upsert_connection(connection.clone())
        .await
        .expect("insert connection");

    let run_a = sync_run(tenant_id, connection.connection_uid);
    let mut run_b = sync_run(tenant_id, connection.connection_uid);
    run_b.sync_run_uid = Uuid::now_v7();
    run_b.started_at = run_a.started_at + Duration::milliseconds(1);

    let (claim_a, claim_b) = tokio::join!(
        repo_a.claim_sync_run(run_a.clone()),
        repo_b.claim_sync_run(run_b.clone())
    );
    let claim_a = claim_a.expect("first claim should complete");
    let claim_b = claim_b.expect("second claim should complete");
    let claimed = [&claim_a, &claim_b]
        .iter()
        .filter(|claim| matches!(claim, SyncRunClaim::Claimed(_)))
        .count();
    let already_running = [&claim_a, &claim_b]
        .iter()
        .filter(|claim| matches!(claim, SyncRunClaim::AlreadyRunning(_)))
        .count();
    assert_eq!(claimed, 1);
    assert_eq!(already_running, 1);

    let claimed_uid = match (&claim_a, &claim_b) {
        (SyncRunClaim::Claimed(run), SyncRunClaim::AlreadyRunning(existing))
        | (SyncRunClaim::AlreadyRunning(existing), SyncRunClaim::Claimed(run)) => {
            assert_eq!(run.sync_run_uid, existing.sync_run_uid);
            run.sync_run_uid
        }
        _ => panic!("expected one claimed run and one already-running result"),
    };
    assert_eq!(
        active_sync_run_count(db.store().pool(), tenant_id, connection.connection_uid).await,
        1
    );
    assert!(
        [run_a.sync_run_uid, run_b.sync_run_uid].contains(&claimed_uid),
        "claim should return one of the racing run IDs"
    );
}

#[tokio::test]
async fn discovery_resolves_tenant_then_scoped_repository_enforces_run_visibility_db_knowledge() {
    // Pins: pre-scope discovery returns only the owner tenant and never bypasses scoped run reads.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated knowledge DB");
    let tenant_a = TenantId::from(Uuid::now_v7());
    let tenant_b = TenantId::from(Uuid::now_v7());
    let repo_a = repository(&db, tenant_a);
    let repo_b = repository(&db, tenant_b);
    let connection_b = connection(tenant_b, "tenant-b");
    repo_b
        .upsert_connection(connection_b.clone())
        .await
        .expect("insert tenant B connection");
    let run_b = sync_run(tenant_b, connection_b.connection_uid);
    repo_b
        .create_sync_run(run_b.clone())
        .await
        .expect("insert tenant B sync run");

    let discovery = discovery(&db);
    assert_eq!(
        discovery
            .resolve_sync_run_tenant(run_b.sync_run_uid)
            .await
            .expect("resolve sync-run tenant"),
        Some(tenant_b)
    );
    assert_eq!(
        discovery
            .resolve_sync_run_tenant(Uuid::now_v7())
            .await
            .expect("resolve missing sync-run tenant"),
        None
    );
    assert_eq!(
        repo_a
            .get_sync_run(run_b.sync_run_uid)
            .await
            .expect("tenant A lookup for tenant B run"),
        None
    );
    assert_eq!(
        repo_b
            .get_sync_run(run_b.sync_run_uid)
            .await
            .expect("tenant B lookup for its run"),
        Some(run_b)
    );
}

#[tokio::test]
async fn document_version_claim_reclaims_stale_row_and_fences_old_token_db_knowledge() {
    // Pins: stale object-ingestion claims are recoverable and terminal updates require the live token.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated knowledge DB");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let repo = repository(&db, tenant_id);

    let connection = connection(tenant_id, "stale-version-claim");
    repo.upsert_connection(connection.clone())
        .await
        .expect("insert connection");
    let object = object(
        tenant_id,
        connection.connection_uid,
        "stale-version-claim",
        "page",
    );
    repo.upsert_object(object.clone())
        .await
        .expect("insert object");

    let mut first_run = sync_run(tenant_id, connection.connection_uid);
    first_run.status = SyncRunStatus::Completed;
    first_run.finished_at = Some(moa_test_support::fixtures::pg_now());
    repo.create_sync_run(first_run.clone())
        .await
        .expect("create first sync run");
    let mut second_run = sync_run(tenant_id, connection.connection_uid);
    second_run.status = SyncRunStatus::Completed;
    second_run.finished_at = Some(moa_test_support::fixtures::pg_now());
    repo.create_sync_run(second_run.clone())
        .await
        .expect("create second sync run");

    let version = DocumentVersion {
        version_uid: Uuid::now_v7(),
        object_uid: object.object_uid,
        parser: "native".to_string(),
        parser_job_id: None,
        content_hash: "hash-stale-claim".to_string(),
        metadata: json!({ "safe": true }),
        created_at: moa_test_support::fixtures::pg_now(),
    };
    let (claimed_version, old_token) = claimed_version_and_token(
        repo.claim_document_version_ingestion(first_run.sync_run_uid, version.clone())
            .await
            .expect("first claim should start"),
    );
    assert_eq!(claimed_version.version_uid, version.version_uid);

    expire_claim_lease(db.store().pool(), version.version_uid).await;
    let (reclaimed_version, new_token) = claimed_version_and_token(
        repo.claim_document_version_ingestion(second_run.sync_run_uid, version.clone())
            .await
            .expect("stale claim should be reclaimed"),
    );
    assert_eq!(reclaimed_version.version_uid, version.version_uid);
    assert_ne!(old_token, new_token);

    assert!(
        repo.fail_document_version_ingestion(
            first_run.sync_run_uid,
            version.version_uid,
            old_token
        )
        .await
        .is_err(),
        "old token must not fail a reclaimed claim"
    );
    let state_after_old_fail = claim_state(db.store().pool(), version.version_uid).await;
    assert_eq!(state_after_old_fail.status, "started");
    assert_eq!(state_after_old_fail.claim_token, new_token);
    assert_eq!(
        state_after_old_fail.claimed_by_sync_run_id,
        second_run.sync_run_uid
    );
    assert_eq!(state_after_old_fail.completed_by_sync_run_id, None);

    assert!(
        repo.complete_document_version_ingestion(
            first_run.sync_run_uid,
            version.version_uid,
            old_token
        )
        .await
        .is_err(),
        "old token must not complete a reclaimed claim"
    );
    repo.complete_document_version_ingestion(
        second_run.sync_run_uid,
        version.version_uid,
        new_token,
    )
    .await
    .expect("new token should complete reclaimed claim");

    let completed_state = claim_state(db.store().pool(), version.version_uid).await;
    assert_eq!(completed_state.status, "completed");
    assert_eq!(completed_state.claim_token, new_token);
    assert_eq!(
        completed_state.completed_by_sync_run_id,
        Some(second_run.sync_run_uid)
    );
}

#[tokio::test]
async fn sync_run_persistence_counters_timelines_filters_and_tenant_rls_db_knowledge() {
    // Pins: sync-run inspection persists status, accumulates counters, orders steps, filters objects, and honors tenant RLS.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated knowledge DB");
    let tenant_a = TenantId::from(Uuid::now_v7());
    let tenant_b = TenantId::from(Uuid::now_v7());
    let repo_a = repository(&db, tenant_a);
    let repo_b = repository(&db, tenant_b);

    let connection_a = connection(tenant_a, "tenant-a");
    let connection_b = connection(tenant_b, "tenant-b");
    repo_a
        .upsert_connection(connection_a.clone())
        .await
        .expect("insert tenant A connection");
    repo_b
        .upsert_connection(connection_b.clone())
        .await
        .expect("insert tenant B connection");

    let mut run_a = sync_run(tenant_a, connection_a.connection_uid);
    run_a.max_records = Some(25);
    let run_b = sync_run(tenant_b, connection_b.connection_uid);
    repo_a
        .create_sync_run(run_a.clone())
        .await
        .expect("create tenant A sync run");
    repo_b
        .create_sync_run(run_b.clone())
        .await
        .expect("create tenant B sync run");

    let created = repo_a
        .get_sync_run(run_a.sync_run_uid)
        .await
        .expect("read tenant A sync run")
        .expect("tenant A run should exist");
    assert_eq!(created.status, SyncRunStatus::Ingesting);
    assert_eq!(created.records_seen, 1);
    assert_eq!(created.max_records, Some(25));
    assert_eq!(created.records_ingested, 0);
    assert_eq!(created.records_failed, 0);

    run_a.status = SyncRunStatus::Completed;
    run_a.parser = Some("llamaparse".to_string());
    run_a.records_seen = 4;
    run_a.records_ingested = 3;
    run_a.records_failed = 1;
    run_a.finished_at = Some(moa_test_support::fixtures::pg_now());
    repo_a
        .update_sync_run(run_a.clone())
        .await
        .expect("update tenant A sync run");
    repo_a
        .add_sync_counters(
            run_a.sync_run_uid,
            KnowledgeSyncCounters {
                records_seen: 2,
                records_changed: 1,
                records_deleted: 0,
                records_ingested: 1,
                records_failed: 0,
                objects_parsed: 1,
                chunks_embedded: 2,
                graph_nodes_upserted: 3,
                graph_edges_upserted: 4,
            },
        )
        .await
        .expect("add first sync counter batch");
    repo_a
        .add_sync_counters(
            run_a.sync_run_uid,
            KnowledgeSyncCounters {
                records_seen: 1,
                records_changed: 2,
                records_deleted: 1,
                records_ingested: 0,
                records_failed: 1,
                objects_parsed: 0,
                chunks_embedded: 5,
                graph_nodes_upserted: 6,
                graph_edges_upserted: 7,
            },
        )
        .await
        .expect("add second sync counter batch");

    let updated = repo_a
        .get_sync_run(run_a.sync_run_uid)
        .await
        .expect("read updated tenant A sync run")
        .expect("tenant A updated run should exist");
    assert_eq!(updated.status, SyncRunStatus::Completed);
    assert_eq!(updated.parser.as_deref(), Some("llamaparse"));
    assert_eq!(updated.max_records, Some(25));
    assert_eq!(updated.records_seen, 7);
    assert_eq!(updated.records_ingested, 4);
    assert_eq!(updated.records_failed, 2);
    assert!(updated.finished_at.is_some());
    assert_eq!(
        stored_counters(db.store().pool(), run_a.sync_run_uid).await,
        StoredCounters {
            records_changed: 3,
            records_deleted: 1,
            objects_parsed: 1,
            chunks_embedded: 7,
            graph_nodes_upserted: 9,
            graph_edges_upserted: 11,
        }
    );

    let page_object = object(
        tenant_a,
        connection_a.connection_uid,
        "tenant-a-page",
        "page",
    );
    let ticket_object = object(
        tenant_a,
        connection_a.connection_uid,
        "tenant-a-ticket",
        "ticket",
    );
    let other_tenant_object = object(
        tenant_b,
        connection_b.connection_uid,
        "tenant-b-page",
        "page",
    );
    repo_a
        .upsert_object(page_object.clone())
        .await
        .expect("insert tenant A page object");
    repo_a
        .upsert_object(ticket_object.clone())
        .await
        .expect("insert tenant A ticket object");
    repo_b
        .upsert_object(other_tenant_object.clone())
        .await
        .expect("insert tenant B object");

    for step in [
        step(
            run_a.sync_run_uid,
            None,
            "provider_triggered",
            IngestionStepStatus::Completed,
            0,
        ),
        step(
            run_a.sync_run_uid,
            Some(page_object.object_uid),
            "content_fetched",
            IngestionStepStatus::Completed,
            10,
        ),
        step(
            run_a.sync_run_uid,
            Some(page_object.object_uid),
            "parse_completed",
            IngestionStepStatus::Completed,
            20,
        ),
        step(
            run_a.sync_run_uid,
            Some(ticket_object.object_uid),
            "graph_upserted",
            IngestionStepStatus::Completed,
            30,
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
            Some(other_tenant_object.object_uid),
            "content_fetched",
            IngestionStepStatus::Completed,
            0,
        ))
        .await
        .expect("record tenant B ingestion step");

    let all_steps = repo_a
        .sync_run_steps(run_a.sync_run_uid, None)
        .await
        .expect("load tenant A sync-run steps");
    assert_eq!(
        all_steps
            .iter()
            .map(|entry| entry.step.as_str())
            .collect::<Vec<_>>(),
        vec![
            "provider_triggered",
            "content_fetched",
            "parse_completed",
            "graph_upserted"
        ]
    );

    let page_steps = repo_a
        .sync_run_steps(run_a.sync_run_uid, Some(page_object.object_uid))
        .await
        .expect("load page object steps");
    assert_eq!(
        page_steps
            .iter()
            .map(|entry| entry.step.as_str())
            .collect::<Vec<_>>(),
        vec!["content_fetched", "parse_completed"]
    );

    let page_objects = repo_a
        .list_objects(
            tenant_a,
            Some(connection_a.connection_uid),
            Some("page"),
            10,
        )
        .await
        .expect("list page objects");
    assert_eq!(page_objects.len(), 1);
    assert_eq!(page_objects[0].object.object_uid, page_object.object_uid);

    assert_eq!(
        repo_a
            .get_sync_run(run_b.sync_run_uid)
            .await
            .expect("tenant A lookup for tenant B sync run"),
        None
    );
    assert_eq!(
        repo_a
            .get_object(other_tenant_object.object_uid)
            .await
            .expect("tenant A lookup for tenant B object"),
        None
    );
    assert!(
        repo_a
            .sync_run_steps(run_b.sync_run_uid, None)
            .await
            .expect("tenant A lookup for tenant B steps")
            .is_empty()
    );
}

fn claimed_version_and_token(claim: DocumentVersionIngestionClaim) -> (DocumentVersion, Uuid) {
    match claim {
        DocumentVersionIngestionClaim::Claimed {
            version,
            claim_token,
        } => (version, claim_token),
        other => panic!("expected claimed document version, got {other:?}"),
    }
}

async fn expire_claim_lease(pool: &sqlx::PgPool, version_uid: Uuid) {
    let result = sqlx::query(
        r#"
        UPDATE moa.knowledge_object_ingestion_claims
        SET lease_expires_at = now() - INTERVAL '1 second',
            updated_at = now() - INTERVAL '1 second'
        WHERE document_version_id = $1
        "#,
    )
    .bind(version_uid)
    .execute(pool)
    .await
    .expect("expire ingestion claim lease");
    assert_eq!(result.rows_affected(), 1);
}

#[derive(Debug, PartialEq, Eq)]
struct ClaimState {
    status: String,
    claim_token: Uuid,
    claimed_by_sync_run_id: Uuid,
    completed_by_sync_run_id: Option<Uuid>,
}

async fn claim_state(pool: &sqlx::PgPool, version_uid: Uuid) -> ClaimState {
    let row = sqlx::query_as::<_, (String, Uuid, Uuid, Option<Uuid>)>(
        r#"
        SELECT status, claim_token, claimed_by_sync_run_id, completed_by_sync_run_id
        FROM moa.knowledge_object_ingestion_claims
        WHERE document_version_id = $1
        "#,
    )
    .bind(version_uid)
    .fetch_one(pool)
    .await
    .expect("read ingestion claim state");
    ClaimState {
        status: row.0,
        claim_token: row.1,
        claimed_by_sync_run_id: row.2,
        completed_by_sync_run_id: row.3,
    }
}

async fn active_sync_run_count(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    connection_uid: Uuid,
) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)
        FROM moa.knowledge_sync_runs
        WHERE tenant_id = $1
          AND connection_id = $2
          AND status IN ('queued', 'provider_syncing', 'provider_synced', 'parse_pending', 'ingesting')
        "#,
    )
    .bind(tenant_id.0)
    .bind(connection_uid)
    .fetch_one(pool)
    .await
    .expect("count active sync runs")
}

#[derive(Debug, PartialEq, Eq)]
struct StoredCounters {
    records_changed: i64,
    records_deleted: i64,
    objects_parsed: i64,
    chunks_embedded: i64,
    graph_nodes_upserted: i64,
    graph_edges_upserted: i64,
}

async fn stored_counters(pool: &sqlx::PgPool, sync_run_uid: Uuid) -> StoredCounters {
    let row = sqlx::query_as::<_, (i64, i64, i64, i64, i64, i64)>(
        r#"
        SELECT records_changed, records_deleted, objects_parsed, chunks_embedded,
               graph_nodes_upserted, graph_edges_upserted
        FROM moa.knowledge_sync_runs
        WHERE sync_run_uid = $1
        "#,
    )
    .bind(sync_run_uid)
    .fetch_one(pool)
    .await
    .expect("read stored sync counters");
    StoredCounters {
        records_changed: row.0,
        records_deleted: row.1,
        objects_parsed: row.2,
        chunks_embedded: row.3,
        graph_nodes_upserted: row.4,
        graph_edges_upserted: row.5,
    }
}

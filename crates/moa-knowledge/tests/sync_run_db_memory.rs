//! DB integration coverage for tenant knowledge sync-run inspection.

use chrono::{Duration, Utc};
use moa_core::RlsContext;
use moa_core::TenantId;
use moa_knowledge::{
    domain::{
        ConnectionStatus, IngestionStepStatus, KnowledgeConnection, KnowledgeIngestionStep,
        KnowledgeObject, KnowledgeSyncCounters, KnowledgeSyncRun, ObjectStatus, SyncRunStatus,
    },
    repository::{KnowledgeRepository, PostgresKnowledgeRepository},
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

fn connection(tenant_id: TenantId, label: &str) -> KnowledgeConnection {
    let now = Utc::now();
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
        started_at: Utc::now(),
        finished_at: None,
    }
}

fn object(
    tenant_id: TenantId,
    connection_uid: Uuid,
    label: &str,
    object_type: &str,
) -> KnowledgeObject {
    KnowledgeObject {
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
        source_updated_at: Some(Utc::now()),
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
        ended_at: Some(started_at + Duration::milliseconds(5)),
        duration_ms: Some(5),
        counters: json!({ "records_seen": 1, "stage": stage }),
        summary: Some(format!("{stage} completed")),
        retry_count: 0,
        error_code: None,
    }
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
    run_a.finished_at = Some(Utc::now());
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

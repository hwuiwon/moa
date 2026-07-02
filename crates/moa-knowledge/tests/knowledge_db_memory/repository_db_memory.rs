//! Postgres repository coverage for tenant knowledge-base RLS and timelines.

use chrono::{Duration, Utc};
use moa_core::RlsContext;
use moa_core::TenantId;
use moa_knowledge::{
    domain::{
        ConnectionStatus, IngestionStepStatus, KnowledgeConnection, KnowledgeIngestionStep,
        KnowledgeObject, KnowledgeSyncRun, ObjectStatus, SyncRunStatus,
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
        provider: "nango".to_string(),
        connector: format!("google-drive-{label}"),
        provider_account_id: format!("provider-account-{label}"),
        credential_ref: format!("vault://tenant/{label}/knowledge"),
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
        started_at: Utc::now(),
        finished_at: None,
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
        ended_at: Some(started_at + Duration::milliseconds(7)),
        duration_ms: Some(7),
        counters: json!({ "records_seen": 1 }),
        summary: Some(format!("{stage} completed without credential details")),
        retry_count: 0,
        error_code: None,
    }
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
    connection_a.last_synced_at = Some(Utc::now());
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

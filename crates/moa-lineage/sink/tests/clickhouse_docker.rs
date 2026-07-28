//! Live coverage for the ClickHouse lineage store against a real server.
//!
//! Pins the pieces the in-process mock cannot: DDL syntax accepted by a real
//! ClickHouse, RowBinary insert round-trips, `fromUnixTimestamp64Micro`
//! filter binding, and lightweight DELETE for tenant offboarding.
//!
//! Run with the compose service:
//! `docker compose --profile clickhouse up -d clickhouse` then
//! `MOA_RUN_CLICKHOUSE_DOCKER_TESTS=1 cargo nextest run -p moa-lineage-sink --run-ignored all -E 'test(clickhouse_store_roundtrip_docker)'`.

use std::time::Duration;

use chrono::{TimeZone, Utc};
use moa_config::ClickHouseConfig;
use moa_core::{
    types::identifiers::SessionId, types::identifiers::StoragePartitionId,
    types::identifiers::TenantId, types::identifiers::UserId,
};
use moa_lineage_core::{
    BackendIntrospection, LineageEvent, RetrievalLineage, RetrievalStage, StageTimings,
};
use moa_lineage_sink::{LineageQueryFilters, LineageStore, MpscSinkConfig, spawn_writer};
use tokio::sync::mpsc;
use uuid::Uuid;

type TestResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[tokio::test]
#[ignore = "requires a local ClickHouse (docker compose --profile clickhouse) and MOA_RUN_CLICKHOUSE_DOCKER_TESTS=1"]
async fn clickhouse_store_roundtrip_docker() -> TestResult<()> {
    if std::env::var("MOA_RUN_CLICKHOUSE_DOCKER_TESTS").as_deref() != Ok("1") {
        return Err("MOA_RUN_CLICKHOUSE_DOCKER_TESTS=1 is required for this test".into());
    }
    let url = std::env::var("MOA_CLICKHOUSE_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:10061".to_string());
    let postgres_url =
        std::env::var("MOA_DATABASE_URL").map_err(|_| "MOA_DATABASE_URL is required")?;

    // Isolated database per run so concurrent runs cannot collide.
    let database = format!("moa_lineage_test_{}", Uuid::now_v7().simple());
    let config = ClickHouseConfig {
        url,
        database: database.clone(),
        user: Some(std::env::var("MOA_CLICKHOUSE_USER").unwrap_or_else(|_| "moa".to_string())),
        password: Some(
            std::env::var("MOA_CLICKHOUSE_PASSWORD").unwrap_or_else(|_| "dev".to_string()),
        ),
        ..ClickHouseConfig::default()
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&postgres_url)
        .await?;
    let store = LineageStore::from_config(Some(&config), pool);
    let clickhouse = store
        .clickhouse()
        .cloned()
        .ok_or("store must select the clickhouse backend")?;

    // Drive rows through the real writer so acceptance, claiming and batching
    // are the production path, not a direct insert call.
    let sink_config = MpscSinkConfig {
        channel_capacity: 16,
        batch_size: 100,
        batch_max_age: Duration::from_secs(3600),
        claim_batch_size: 100,
        lease_ttl: Duration::from_secs(60),
        max_pending_age: Duration::from_secs(300),
        drain_timeout: Duration::from_secs(30),
    };
    let (tx, rx) = mpsc::channel::<LineageEvent>(16);
    let handle = spawn_writer(rx, sink_config, store).await?;

    let tenant_id = Uuid::now_v7();
    let partition = StoragePartitionId::for_tenant(TenantId::from(tenant_id));
    let session_id = SessionId::new();
    let early_turn = Uuid::now_v7();
    let late_turn = Uuid::now_v7();
    let early_ts = Utc
        .with_ymd_and_hms(2026, 7, 8, 10, 0, 0)
        .single()
        .ok_or("ts")?;
    let late_ts = Utc
        .with_ymd_and_hms(2026, 7, 8, 11, 0, 0)
        .single()
        .ok_or("ts")?;
    for (turn_id, ts) in [(early_turn, early_ts), (late_turn, late_ts)] {
        tx.send(LineageEvent::Retrieval(RetrievalLineage {
            turn_id: moa_lineage_core::TurnId(turn_id),
            session_id,
            storage_partition_id: partition.clone(),
            user_id: UserId::new("clickhouse-docker-user"),
            scope: moa_memory_types::MemoryScope::Tenant {
                tenant_id: TenantId::from(tenant_id),
            },
            ts,
            query_original: "what is oauth".to_string(),
            query_expansions: Vec::new(),
            vector_hits: Vec::new(),
            graph_paths: Vec::new(),
            fusion_scores: Vec::new(),
            rerank_scores: Vec::new(),
            top_k: Vec::new(),
            searched_scopes: Vec::new(),
            selected_hits: Vec::new(),
            filters: serde_json::Value::Null,
            timings: StageTimings::default(),
            introspection: BackendIntrospection::default(),
            stage: RetrievalStage::Single,
        }))
        .await?;
    }
    drop(tx);
    let stats = handle.shutdown().await?;
    assert_eq!(stats.written, 2, "both rows must flush to ClickHouse");

    // Explain sees both rows for the session in timestamp order.
    let explain = clickhouse.explain_records(&partition, session_id.0).await?;
    assert_eq!(explain.len(), 2);
    assert_eq!(explain[0].turn_id, early_turn);
    assert_eq!(explain[1].turn_id, late_turn);
    assert_eq!(explain[0].tenant_id, Some(TenantId::from(tenant_id)));
    assert_eq!(
        explain[0].payload["record"]["query_original"],
        serde_json::json!("what is oauth")
    );

    // Typed query: time filter plus descending order and limit.
    let filtered = clickhouse
        .query_records(
            &partition,
            LineageQueryFilters {
                from_time: Some(late_ts - chrono::Duration::minutes(1)),
                descending: true,
                limit: 10,
                ..LineageQueryFilters::default()
            },
        )
        .await?;
    assert_eq!(filtered.len(), 1, "time filter must exclude the early row");
    assert_eq!(filtered[0].turn_id, late_turn);

    // Trace payloads decode back to the retrieval event.
    let traces = clickhouse.trace_payloads(&partition, early_turn, 1).await?;
    assert_eq!(traces.len(), 1);

    // DSAR subject search matches payload content case-insensitively.
    let dsar = clickhouse
        .load_dsar_export_records(&partition, "OAUTH")
        .await?;
    assert_eq!(dsar.len(), 2);

    // Offboarding delete empties the partition.
    clickhouse.delete_partition_rows(&partition).await?;
    let after_delete = clickhouse.explain_records(&partition, session_id.0).await?;
    assert!(after_delete.is_empty(), "delete must remove tenant rows");

    Ok(())
}

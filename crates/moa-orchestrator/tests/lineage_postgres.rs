//! Postgres lineage sink smoke coverage for the cloud orchestrator selector.

use anyhow::Result;
use chrono::Utc;
use moa_core::{MemoryScope, MoaConfig, SessionId, UserId, WorkspaceId};
use moa_lineage_core::{
    BackendIntrospection, LineageEvent, RetrievalLineage, RetrievalStage, StageTimings, TurnId,
};
use moa_orchestrator::lineage::build_lineage_sink_from_env_value;
use uuid::Uuid;

#[tokio::test]
async fn postgres_lineage_sink_writes_rows() -> Result<()> {
    // Pins: MOA_LINEAGE_SINK=postgres selects MpscSink and drains one lineage row to Postgres.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let workspace_name = "lineage-postgres-test";
    sqlx::query("DELETE FROM analytics.turn_lineage WHERE workspace_id = $1")
        .bind(workspace_name)
        .execute(&pool)
        .await?;

    let journal_dir = tempfile::tempdir()?;
    let mut config = MoaConfig::default();
    config.observability.lineage.journal_path = journal_dir
        .path()
        .join("lineage-journal")
        .display()
        .to_string();

    let runtime =
        build_lineage_sink_from_env_value(&config, pool.clone(), Some("postgres")).await?;
    let turn_id = TurnId::new_v7();
    let session_id = SessionId::new();
    let workspace_id = WorkspaceId::new(workspace_name);
    let event = LineageEvent::Retrieval(RetrievalLineage {
        turn_id,
        session_id,
        workspace_id: workspace_id.clone(),
        user_id: UserId::new("test-user"),
        scope: MemoryScope::Workspace { workspace_id },
        ts: Utc::now(),
        query_original: "lineage smoke".to_string(),
        query_expansions: Vec::new(),
        vector_hits: Vec::new(),
        graph_paths: Vec::new(),
        fusion_scores: Vec::new(),
        rerank_scores: Vec::new(),
        top_k: vec![Uuid::now_v7()],
        timings: StageTimings::default(),
        introspection: BackendIntrospection::default(),
        stage: RetrievalStage::Single,
    });

    runtime.handle.record(serde_json::to_value(event)?);
    let writer = runtime
        .writer
        .as_ref()
        .expect("postgres lineage sink should return a writer handle");
    let stats = writer.shutdown().await?;

    assert_eq!(stats.written, 1);

    let count: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM analytics.turn_lineage
        WHERE turn_id = $1 AND session_id = $2
        "#,
    )
    .bind(turn_id.0)
    .bind(session_id.0)
    .fetch_one(&pool)
    .await?;
    assert_eq!(count, 1);

    sqlx::query("DELETE FROM analytics.turn_lineage WHERE workspace_id = $1")
        .bind(workspace_name)
        .execute(&pool)
        .await?;

    Ok(())
}

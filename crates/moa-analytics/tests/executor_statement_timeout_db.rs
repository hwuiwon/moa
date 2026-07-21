//! DB coverage for the analytics executor: the per-query Postgres budget and
//! the `citation_precision` dataset's retrieval/citation lineage join.

use moa_analytics::AnalyticsService;
use moa_core::types::identifiers::{SessionId, StoragePartitionId, TenantId, UserId};
use moa_lineage_core::{
    Citation, CitationLineage, LineageEvent, RecordKind, TurnId, VerifierResult,
};
use moa_test_support::postgres::{TestDb, bootstrap_test_db};
use moa_wire::analytics::{
    AnalyticsAggregation, AnalyticsCell, AnalyticsDimension, AnalyticsFilter,
    AnalyticsFilterOperator, AnalyticsMeasure, AnalyticsQueryRequest,
};
use sqlx::PgPool;
use uuid::Uuid;

async fn configured_test_db() -> Option<TestDb> {
    std::env::var_os("MOA_DATABASE_URL")?;
    Some(
        bootstrap_test_db()
            .await
            .expect("bootstrap Postgres test database"),
    )
}

#[tokio::test]
#[ignore = "requires MOA_DATABASE_URL and a reachable Postgres instance"]
async fn analytics_query_applies_statement_timeout_and_runs_db() {
    // Pins: the Postgres analytics executor runs each query inside a tenant-scoped
    // transaction with a bounded statement_timeout set, and the query path
    // succeeds against the empty read models.
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let tenant = TenantId::new();
    let request = AnalyticsQueryRequest {
        dataset: "sessions".to_string(),
        tenant_id: Some(tenant),
        dimensions: vec![AnalyticsDimension {
            field: "channel".to_string(),
            alias: None,
        }],
        measures: vec![AnalyticsMeasure {
            field: None,
            aggregation: moa_wire::analytics::AnalyticsAggregation::Count,
            alias: None,
        }],
        filters: vec![AnalyticsFilter {
            field: "created_at".to_string(),
            operator: AnalyticsFilterOperator::Gte,
            value: Some(AnalyticsCell::String(
                (chrono::Utc::now() - chrono::Duration::days(1)).to_rfc3339(),
            )),
        }],
        order_by: Vec::new(),
        limit: Some(10),
    };

    let response = AnalyticsService::new()
        .with_statement_timeout_ms(5_000)
        .query(test_db.store().pool(), request)
        .await
        .expect("analytics query runs under the statement-timeout budget");
    assert_eq!(response.metadata.row_count, 0, "empty read model");
}

#[tokio::test]
#[ignore = "requires MOA_DATABASE_URL and a reachable Postgres instance"]
async fn statement_timeout_cancels_a_slow_query_db() {
    // Pins: the per-transaction statement_timeout the executor sets actually
    // cancels a runaway query server-side (SQLSTATE 57014), so an unbounded
    // ordered-percentile scan cannot hold a connection indefinitely.
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let mut tx = test_db
        .store()
        .pool()
        .begin()
        .await
        .expect("begin transaction");
    // Same mechanism as executor.rs: SET LOCAL statement_timeout for this tx.
    sqlx::query("SELECT set_config('statement_timeout', $1, true)")
        .bind("100")
        .execute(tx.as_mut())
        .await
        .expect("set statement timeout");

    let error = sqlx::query("SELECT pg_sleep(3)")
        .execute(tx.as_mut())
        .await
        .expect_err("a query longer than the timeout must be cancelled");
    let cancelled = error
        .as_database_error()
        .and_then(|db_error| db_error.code().map(|code| code.into_owned()))
        .as_deref()
        == Some("57014")
        || error
            .to_string()
            .to_lowercase()
            .contains("statement timeout");
    assert!(
        cancelled,
        "expected a statement-timeout cancellation, got: {error}"
    );

    let _ = tx.rollback().await;
}

/// Seeds one `moa.retrieval_lineage` row the way the retrieval enrichment
/// writer does, with an explicit tenant id so the runtime-column trigger does
/// not have to derive it.
#[allow(clippy::too_many_arguments)]
async fn seed_retrieval_hit(
    pool: &PgPool,
    tenant: TenantId,
    storage_partition_id: &StoragePartitionId,
    user_id: &UserId,
    session_id: SessionId,
    turn_id: Uuid,
    node_uid: Uuid,
    chunk_uid: Option<Uuid>,
    rank: i32,
    retrieved_at: chrono::DateTime<chrono::Utc>,
) {
    sqlx::query(
        "INSERT INTO moa.retrieval_lineage \
         (tenant_id, contact_id, storage_partition_id, user_id, session_id, \
          turn_seq, turn_id, uid, chunk_uid, document_version_uid, rank, retrieved_at) \
         VALUES ($1, NULL, $2, $3, $4, 1, $5, $6, $7, NULL, $8, $9)",
    )
    .bind(tenant.0)
    .bind(storage_partition_id.as_str())
    .bind(user_id.as_str())
    .bind(session_id.0)
    .bind(turn_id)
    .bind(node_uid)
    .bind(chunk_uid)
    .bind(rank)
    .bind(retrieved_at)
    .execute(pool)
    .await
    .expect("seed retrieval lineage row");
}

/// Persists one durable Citation lineage row for `turn_id`, serializing the
/// real `LineageEvent::Citation` record so the payload shape stays exactly
/// what the lineage sink writer lands in `analytics.turn_lineage`.
async fn seed_citation_lineage(
    pool: &PgPool,
    storage_partition_id: &StoragePartitionId,
    user_id: &UserId,
    session_id: SessionId,
    turn_id: Uuid,
    citations: Vec<Citation>,
) {
    let record = CitationLineage {
        turn_id: TurnId(turn_id),
        session_id,
        storage_partition_id: storage_partition_id.clone(),
        user_id: user_id.clone(),
        ts: chrono::Utc::now(),
        answer_text: "OAuth uses access tokens.".to_string(),
        answer_event_id: None,
        answer_event_sequence_num: None,
        answer_sentence_offsets: vec![(0, 25)],
        citations,
        vendor_used: None,
        verifier_used: Some("cascade-bm25+lexical-overlap".to_string()),
    };
    let payload = serde_json::to_value(LineageEvent::Citation(record))
        .expect("serialize citation lineage payload");
    sqlx::query(
        "INSERT INTO analytics.turn_lineage \
         (turn_id, session_id, user_id, storage_partition_id, ts, tier, \
          record_kind, payload, integrity_hash) \
         VALUES ($1, $2, $3, $4, NOW(), 1, $5, $6, $7)",
    )
    .bind(turn_id)
    .bind(session_id.0)
    .bind(user_id.as_str())
    .bind(storage_partition_id.as_str())
    .bind(RecordKind::Citation.as_i16())
    .bind(payload)
    .bind(vec![0_u8; 32])
    .execute(pool)
    .await
    .expect("seed citation lineage row");
}

fn verified_citation(source_chunk_id: Uuid, source_node_uid: Option<Uuid>) -> Citation {
    Citation {
        answer_span: 0,
        answer_span_bytes: None,
        source_chunk_id,
        source_node_uid,
        cited_text: Some("OAuth uses access tokens.".to_string()),
        vendor_score: None,
        verifier: VerifierResult {
            verified: true,
            bm25_score: Some(1.0),
            nli_entailment: None,
            nli_contradiction: None,
            method: "bm25+lexical_overlap".to_string(),
        },
    }
}

fn measure(field: &str, aggregation: AnalyticsAggregation, alias: &str) -> AnalyticsMeasure {
    AnalyticsMeasure {
        field: Some(field.to_string()),
        aggregation,
        alias: Some(alias.to_string()),
    }
}

fn numeric_cell(cell: &AnalyticsCell) -> f64 {
    match cell {
        AnalyticsCell::Number(number) => number.as_f64().expect("numeric cell fits f64"),
        other => panic!("expected a numeric cell, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires MOA_DATABASE_URL and a reachable Postgres instance"]
async fn citation_precision_counts_cited_injected_hits_db() {
    // Pins: the citation_precision dataset treats each rank <= 3 retrieval
    // lineage row as one injected hit, marks a hit cited when the same turn's
    // durable Citation lineage references its chunk uid (or graph node uid),
    // and excludes hits ranked below the rendered evidence window — so
    // count/sum/avg produce injected_hits, cited_hits, and the citation rate.
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let pool = test_db.store().pool();
    let tenant = TenantId::new();
    let storage_partition_id = StoragePartitionId::for_tenant(tenant);
    let user_id = UserId::new("citation-precision-user");
    let session_id = SessionId::new();
    let turn_id = Uuid::now_v7();
    let cited_node_uid = Uuid::now_v7();
    let cited_chunk_uid = Uuid::now_v7();
    let uncited_node_uid = Uuid::now_v7();
    let deep_node_uid = Uuid::now_v7();
    let now = chrono::Utc::now();

    // Two hits inside the injection window (one later cited, one not) plus a
    // rank-4 hit that was retrieved but never rendered.
    seed_retrieval_hit(
        pool,
        tenant,
        &storage_partition_id,
        &user_id,
        session_id,
        turn_id,
        cited_node_uid,
        Some(cited_chunk_uid),
        1,
        now,
    )
    .await;
    seed_retrieval_hit(
        pool,
        tenant,
        &storage_partition_id,
        &user_id,
        session_id,
        turn_id,
        uncited_node_uid,
        None,
        2,
        now,
    )
    .await;
    seed_retrieval_hit(
        pool,
        tenant,
        &storage_partition_id,
        &user_id,
        session_id,
        turn_id,
        deep_node_uid,
        None,
        4,
        now,
    )
    .await;
    // The answer cites the chunk-backed hit by its knowledge chunk uid, the
    // key `emit_context_lineage` puts on evidence-backed ChunkRefs. The node
    // uid is deliberately absent so the chunk-uid match path is load-bearing.
    seed_citation_lineage(
        pool,
        &storage_partition_id,
        &user_id,
        session_id,
        turn_id,
        vec![verified_citation(cited_chunk_uid, None)],
    )
    .await;

    let request = AnalyticsQueryRequest {
        dataset: "citation_precision".to_string(),
        tenant_id: Some(tenant),
        dimensions: Vec::new(),
        measures: vec![
            AnalyticsMeasure {
                field: None,
                aggregation: AnalyticsAggregation::Count,
                alias: Some("injected_hits".to_string()),
            },
            measure("cited_hit", AnalyticsAggregation::Sum, "cited_hits"),
            measure("cited_hit", AnalyticsAggregation::Avg, "citation_rate"),
            measure(
                "cited_verified_hit",
                AnalyticsAggregation::Avg,
                "verified_citation_rate",
            ),
        ],
        filters: vec![AnalyticsFilter {
            field: "retrieved_at".to_string(),
            operator: AnalyticsFilterOperator::Gte,
            value: Some(AnalyticsCell::String(
                (now - chrono::Duration::hours(1)).to_rfc3339(),
            )),
        }],
        order_by: Vec::new(),
        limit: Some(10),
    };

    let response = AnalyticsService::new()
        .query(pool, request)
        .await
        .expect("citation precision query runs");
    assert_eq!(response.rows.len(), 1, "one aggregate row");
    let row = &response.rows[0];
    assert_eq!(numeric_cell(&row[0]), 2.0, "injected hits (rank <= 3)");
    assert_eq!(numeric_cell(&row[1]), 1.0, "cited hits");
    assert_eq!(numeric_cell(&row[2]), 0.5, "citation rate");
    assert_eq!(numeric_cell(&row[3]), 0.5, "verified citation rate");
}

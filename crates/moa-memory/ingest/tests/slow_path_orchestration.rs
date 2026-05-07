//! Integration tests for the slow-path ingestion orchestration.

mod support;

use std::sync::Arc;

use chrono::Duration;
use moa_memory_ingest::ingest_turn_direct_with_ctx;
use serde::Deserialize;
use uuid::Uuid;

use support::{
    TEST_LOCK, active_fact_rows, configured_test_db, contradiction_edge_count,
    create_changelog_payloads, create_fact, fact_rows, fixed_time, ingest_ctx, ingest_ctx_with_pii,
    node_confidence, node_valid_to, turn,
};

#[derive(Debug, Deserialize)]
struct ExpectedFacts {
    facts: Vec<ExpectedFact>,
}

#[derive(Debug, Deserialize)]
struct ExpectedFact {
    subject: String,
    predicate: String,
    object: String,
    pii_class: String,
    confidence: f64,
}

#[tokio::test]
async fn slow_path_ingests_simple_document_and_writes_expected_facts_to_graph() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let workspace_id = Uuid::now_v7();
    let ctx = ingest_ctx(test_db.store().pool(), workspace_id);
    let expected: ExpectedFacts =
        serde_json::from_str(include_str!("support/fixtures/expected_facts_simple.json"))
            .expect("expected facts parse");

    let report = ingest_turn_direct_with_ctx(
        ctx,
        turn(
            workspace_id,
            include_str!("support/fixtures/document_simple.md"),
            1,
        ),
    )
    .await
    .expect("slow path ingests simple document");

    assert_eq!(report.inserted, expected.facts.len());
    assert_eq!(report.superseded, 0);
    assert_eq!(report.skipped, 0);
    assert_eq!(report.failed, 0);
    let rows = fact_rows(test_db.store().pool(), workspace_id).await;
    assert_eq!(rows.len(), expected.facts.len());
    for expected in expected.facts {
        let row = rows
            .iter()
            .find(|row| {
                row.properties_summary
                    .as_ref()
                    .and_then(|properties| properties.get("subject"))
                    .and_then(serde_json::Value::as_str)
                    == Some(expected.subject.as_str())
            })
            .expect("expected fact should be present");
        let properties = row
            .properties_summary
            .as_ref()
            .expect("fact properties projected");
        assert_eq!(properties["predicate"], expected.predicate);
        assert_eq!(properties["object"], expected.object);
        assert_eq!(row.pii_class.as_str(), expected.pii_class);
        assert_eq!(
            node_confidence(test_db.store().pool(), workspace_id, row.uid).await,
            expected.confidence
        );
    }
}

#[tokio::test]
async fn slow_path_ingests_document_with_contradictions_and_emits_contradicts_edges() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let workspace_id = Uuid::now_v7();
    let old_uid = create_fact(
        test_db.store().pool(),
        workspace_id,
        "API runs_on_port 3000",
        fixed_time() - Duration::days(1),
    )
    .await;
    let ctx = ingest_ctx(test_db.store().pool(), workspace_id);

    let report = ingest_turn_direct_with_ctx(
        ctx,
        turn(
            workspace_id,
            "Fact: API runs_on_port 3000\nFact: API runs_on_port 8080",
            2,
        ),
    )
    .await
    .expect("slow path handles contradictory document");

    assert_eq!(report.inserted, 0);
    assert_eq!(report.superseded, 1);
    assert_eq!(report.skipped, 1);
    assert_eq!(report.failed, 0);
    assert!(
        node_valid_to(test_db.store().pool(), workspace_id, old_uid)
            .await
            .is_some()
    );
    assert_eq!(
        contradiction_edge_count(test_db.store().pool(), workspace_id).await,
        1
    );
}

#[tokio::test]
async fn slow_path_ingests_supersession_when_new_fact_replaces_existing() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let workspace_id = Uuid::now_v7();
    let old_uid = create_fact(
        test_db.store().pool(),
        workspace_id,
        "API runs_on_port 3000",
        fixed_time() - Duration::days(1),
    )
    .await;
    let ctx = ingest_ctx(test_db.store().pool(), workspace_id);

    let report = ingest_turn_direct_with_ctx(
        ctx,
        turn(
            workspace_id,
            include_str!("support/fixtures/document_supersedes_existing.md"),
            3,
        ),
    )
    .await
    .expect("slow path supersedes existing fact");

    assert_eq!(report.inserted, 0);
    assert_eq!(report.superseded, 1);
    assert_eq!(report.skipped, 0);
    assert_eq!(report.failed, 0);
    assert!(
        node_valid_to(test_db.store().pool(), workspace_id, old_uid)
            .await
            .is_some()
    );
    let rows = fact_rows(test_db.store().pool(), workspace_id).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(
        active_fact_rows(test_db.store().pool(), workspace_id)
            .await
            .iter()
            .map(|row| row.name.as_str())
            .collect::<Vec<_>>(),
        vec!["API"]
    );
}

#[tokio::test]
async fn slow_path_skips_chunks_that_yield_no_extractable_facts() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let workspace_id = Uuid::now_v7();
    let ctx = ingest_ctx(test_db.store().pool(), workspace_id);

    let report = ingest_turn_direct_with_ctx(
        ctx,
        turn(
            workspace_id,
            "Fact: API runs_on_port 3000\nHello, hope you're well.",
            4,
        ),
    )
    .await
    .expect("slow path skips non-factual text");

    assert_eq!(report.inserted, 1);
    assert_eq!(report.superseded, 0);
    assert_eq!(report.skipped, 0);
    assert_eq!(report.failed, 0);
    let rows = active_fact_rows(test_db.store().pool(), workspace_id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "API");
}

#[tokio::test]
async fn slow_path_is_atomic_when_a_chunk_fails_partway_through_extraction() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let workspace_id = Uuid::now_v7();
    let ctx = ingest_ctx_with_pii(
        test_db.store().pool(),
        workspace_id,
        Arc::new(support::FailOnNthPiiClassifier::new(2)),
    );

    let error = ingest_turn_direct_with_ctx(
        ctx,
        turn(
            workspace_id,
            "Fact: API runs_on_port 3000\nFact: worker_queue uses Redis",
            5,
        ),
    )
    .await
    .expect_err("pre-write classifier failure should abort slow path");

    let error_text = format!("{error:?}");
    assert!(
        error_text.contains("intentional pre-write failure"),
        "{error_text}"
    );
    assert!(
        fact_rows(test_db.store().pool(), workspace_id)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn slow_path_emits_lineage_events_for_each_extracted_fact() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let workspace_id = Uuid::now_v7();
    let ctx = ingest_ctx(test_db.store().pool(), workspace_id);
    let session_turn = turn(
        workspace_id,
        include_str!("support/fixtures/document_simple.md"),
        6,
    );
    let session_id = session_turn.session_id.to_string();

    let report = ingest_turn_direct_with_ctx(ctx, session_turn)
        .await
        .expect("slow path writes lineage changelog rows");

    assert_eq!(report.inserted, 3);
    let payloads = create_changelog_payloads(test_db.store().pool(), workspace_id).await;
    assert_eq!(payloads.len(), 3);
    for payload in &payloads {
        let after = payload
            .get("after")
            .expect("create payload has after object");
        assert_eq!(after["source_session_id"], session_id);
        assert_eq!(after["source_turn_seq"], 6);
        assert_eq!(after["source_chunk"], 0);
        assert!(after.get("summary").is_some(), "{after}");
    }
}

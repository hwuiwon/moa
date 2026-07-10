//! Integration tests for the slow-path ingestion orchestration.

mod support;

use std::collections::HashSet;
use std::sync::Arc;

use chrono::Duration;
use moa_memory_graph::{EdgeLabel, GraphWalkScoring, NodeLabel};
use moa_memory_ingest::{
    DeterministicEntityMergeVerifier, IngestError, ScriptedFactExtractor, TurnChunk, extract_facts,
    ingest_turn_direct_with_ctx,
};
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use support::{
    SLOW_PATH_CONTACT_ID, TEST_LOCK, active_tenant_entity_rows, active_tenant_fact_rows,
    active_user_entity_rows, active_user_fact_rows, configured_test_db, contradiction_edge_count,
    create_changelog_payloads, create_fact, entity_resolution_edges, entity_rows, fact_rows,
    fixed_time, ingest_ctx, ingest_ctx_with_pii, node_confidence, node_ranking_state,
    node_valid_to, relates_to_edges, set_node_ranking_state, supersede_protocol_count,
    supersedes_edge_exists, turn, user_fact_rows,
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

#[derive(Debug, PartialEq, Eq)]
struct DurableIngestCounts {
    graph_nodes: i64,
    graph_creates: i64,
    vectors: i64,
    dedup_rows: i64,
}

#[tokio::test]
async fn slow_path_ingests_simple_document_and_writes_expected_facts_to_graph() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let storage_partition_id = Uuid::now_v7();
    let ctx = ingest_ctx(test_db.store().pool(), storage_partition_id).await;
    let expected: ExpectedFacts =
        serde_json::from_str(include_str!("support/fixtures/expected_facts_simple.json"))
            .expect("expected facts parse");

    let report = ingest_turn_direct_with_ctx(
        ctx,
        turn(
            storage_partition_id,
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
    assert_eq!(
        active_tenant_fact_rows(test_db.store().pool(), storage_partition_id)
            .await
            .len(),
        0
    );
    let rows = active_user_fact_rows(test_db.store().pool(), storage_partition_id).await;
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
            node_confidence(test_db.store().pool(), storage_partition_id, row.uid).await,
            expected.confidence
        );
        assert_eq!(row.scope.as_str(), "contact");
        assert_eq!(row.contact_id.as_deref(), Some(SLOW_PATH_CONTACT_ID));
    }
    assert_eq!(
        active_user_entity_rows(test_db.store().pool(), storage_partition_id)
            .await
            .len(),
        6
    );
    assert_eq!(
        active_tenant_entity_rows(test_db.store().pool(), storage_partition_id)
            .await
            .len(),
        0
    );
}

#[tokio::test]
async fn duplicate_direct_ingest_attempt_skips_without_duplicate_durable_rows() {
    // Pins: same-turn direct ingestion replay is idempotent at the DB graph/vector/dedup boundary.
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let storage_partition_id = Uuid::now_v7();
    let ctx = ingest_ctx(test_db.store().pool(), storage_partition_id).await;
    let session_turn = turn(storage_partition_id, "Fact: API uses Redis", 12);

    let first = ingest_turn_direct_with_ctx(ctx.clone(), session_turn.clone())
        .await
        .expect("first direct slow-path ingestion should insert");

    assert_eq!(first.inserted, 1);
    assert_eq!(first.superseded, 0);
    assert_eq!(first.skipped, 0);
    assert_eq!(first.failed, 0);
    let after_first = durable_ingest_counts(test_db.store().pool(), storage_partition_id).await;
    assert_eq!(
        after_first,
        DurableIngestCounts {
            graph_nodes: 3,
            graph_creates: 5,
            vectors: 1,
            dedup_rows: 1,
        }
    );

    let second = ingest_turn_direct_with_ctx(ctx, session_turn)
        .await
        .expect("duplicate direct slow-path ingestion should skip");

    assert_eq!(second.inserted, 0);
    assert_eq!(second.superseded, 0);
    assert_eq!(second.skipped, 1);
    assert_eq!(second.failed, 0);
    assert_eq!(
        durable_ingest_counts(test_db.store().pool(), storage_partition_id).await,
        after_first,
        "duplicate direct ingestion must not add graph, vector, or dedup rows"
    );
}

#[tokio::test]
async fn reobserved_duplicate_fact_reinforces_survivor_once_per_turn() {
    // Pins: a fact re-observed in a later turn confirms its surviving node —
    // confidence steps by exactly the reinforcement step, the base_confidence
    // decay anchor clears, and last_accessed_at advances — while replaying the
    // same turn takes the dedup early-return and leaves the boost untouched.
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let pool = test_db.store().pool();
    let storage_partition_id = Uuid::now_v7();
    let old_uid = create_fact(
        pool,
        storage_partition_id,
        "API runs_on_port 3000",
        fixed_time() - Duration::days(30),
    )
    .await;
    let stale_access = fixed_time() - Duration::days(30);
    set_node_ranking_state(
        pool,
        storage_partition_id,
        old_uid,
        0.6,
        Some(0.9),
        stale_access,
    )
    .await;

    let duplicate_turn = turn(storage_partition_id, "Fact: API runs_on_port 3000", 2);
    let report = ingest_turn_direct_with_ctx(
        ingest_ctx(pool, storage_partition_id).await,
        duplicate_turn.clone(),
    )
    .await
    .expect("re-observed fact should ingest");

    assert_eq!(report.reinforced, 1);
    assert_eq!(report.inserted, 0);
    assert_eq!(report.superseded, 0);
    assert_eq!(report.skipped, 0);
    assert_eq!(report.failed, 0);

    let rows = active_user_fact_rows(pool, storage_partition_id).await;
    assert_eq!(rows.len(), 1, "reinforcement must not add fact nodes");
    assert_eq!(rows[0].uid, old_uid);

    let (confidence, base_confidence, last_accessed_at) =
        node_ranking_state(pool, storage_partition_id, old_uid).await;
    assert!(
        (confidence - 0.7).abs() < 1e-9,
        "confidence must step from 0.6 by exactly 0.1, got {confidence}"
    );
    assert_eq!(
        base_confidence, None,
        "decay anchor must clear so the next decay re-anchors from the boosted value"
    );
    assert!(
        last_accessed_at > stale_access,
        "last_accessed_at must advance on reinforcement"
    );

    let replay =
        ingest_turn_direct_with_ctx(ingest_ctx(pool, storage_partition_id).await, duplicate_turn)
            .await
            .expect("replayed turn should ingest");

    assert_eq!(replay.reinforced, 0);
    assert_eq!(replay.skipped, 1);
    let (confidence_after_replay, _, _) =
        node_ranking_state(pool, storage_partition_id, old_uid).await;
    assert!(
        (confidence_after_replay - 0.7).abs() < 1e-9,
        "replay must not boost confidence twice, got {confidence_after_replay}"
    );
}

#[tokio::test]
async fn slow_path_respects_user_default_and_tenant_shared_scope_markers() {
    // Pins: unmarked and user-private slow-path facts stay user scoped, while explicit
    // tenant-shared markers write tenant rows with the marker stripped.
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let storage_partition_id = Uuid::now_v7();
    let ctx = ingest_ctx(test_db.store().pool(), storage_partition_id).await;

    let report = ingest_turn_direct_with_ctx(
        ctx,
        turn(
            storage_partition_id,
            "Fact: editor_theme uses solarized\nFact: contact private shell uses zsh\nFact: tenant shared API runs_on_port 3000",
            10,
        ),
    )
    .await
    .expect("slow path ingests mixed-scope document");

    assert_eq!(report.inserted, 3);
    assert_eq!(report.superseded, 0);
    assert_eq!(report.skipped, 0);
    assert_eq!(report.failed, 0);
    let user_rows = active_user_fact_rows(test_db.store().pool(), storage_partition_id).await;
    let tenant_rows = active_tenant_fact_rows(test_db.store().pool(), storage_partition_id).await;
    assert_eq!(user_rows.len(), 2);
    assert_eq!(tenant_rows.len(), 1);
    assert_eq!(
        user_rows
            .iter()
            .map(|row| (
                row.name.as_str(),
                row.scope.as_str(),
                row.contact_id.as_deref()
            ))
            .collect::<Vec<_>>(),
        vec![
            ("editor_theme", "contact", Some(SLOW_PATH_CONTACT_ID)),
            ("shell", "contact", Some(SLOW_PATH_CONTACT_ID)),
        ]
    );
    assert_eq!(tenant_rows[0].name, "API");
    assert_eq!(tenant_rows[0].scope.as_str(), "tenant");
    assert_eq!(tenant_rows[0].contact_id.as_deref(), None);
    let tenant_properties = tenant_rows[0]
        .properties_summary
        .as_ref()
        .expect("tenant fact properties projected");
    assert_eq!(tenant_properties["summary"], "API runs_on_port 3000");
    assert_eq!(
        active_user_entity_rows(test_db.store().pool(), storage_partition_id)
            .await
            .len(),
        4
    );
    assert_eq!(
        active_tenant_entity_rows(test_db.store().pool(), storage_partition_id)
            .await
            .len(),
        2
    );
}

#[tokio::test]
async fn slow_path_ingests_document_with_contradictions_and_uses_supersedes_edges() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let storage_partition_id = Uuid::now_v7();
    let old_uid = create_fact(
        test_db.store().pool(),
        storage_partition_id,
        "API runs_on_port 3000",
        fixed_time() - Duration::days(1),
    )
    .await;
    let ctx = ingest_ctx(test_db.store().pool(), storage_partition_id).await;

    let report = ingest_turn_direct_with_ctx(
        ctx,
        turn(
            storage_partition_id,
            "Fact: API runs_on_port 3000\nFact: API runs_on_port 8080",
            2,
        ),
    )
    .await
    .expect("slow path handles contradictory document");

    assert_eq!(report.inserted, 0);
    assert_eq!(report.superseded, 1);
    // The restated "3000" fact reinforces the seeded node before the "8080"
    // fact supersedes it; nothing is silently skipped.
    assert_eq!(report.reinforced, 1);
    assert_eq!(report.skipped, 0);
    assert_eq!(report.failed, 0);
    assert!(
        node_valid_to(test_db.store().pool(), storage_partition_id, old_uid)
            .await
            .is_some()
    );
    let user_rows = active_user_fact_rows(test_db.store().pool(), storage_partition_id).await;
    assert_eq!(user_rows.len(), 1);
    assert_eq!(
        active_tenant_fact_rows(test_db.store().pool(), storage_partition_id)
            .await
            .len(),
        0
    );
    assert!(
        supersedes_edge_exists(
            test_db.store().pool(),
            storage_partition_id,
            old_uid,
            user_rows[0].uid
        )
        .await
    );
    assert_eq!(
        supersede_protocol_count(test_db.store().pool(), storage_partition_id).await,
        1
    );
    assert_eq!(
        contradiction_edge_count(test_db.store().pool(), storage_partition_id).await,
        0
    );
}

#[tokio::test]
async fn slow_path_ingests_supersession_when_new_fact_replaces_existing() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let storage_partition_id = Uuid::now_v7();
    let old_uid = create_fact(
        test_db.store().pool(),
        storage_partition_id,
        "API runs_on_port 3000",
        fixed_time() - Duration::days(1),
    )
    .await;
    let ctx = ingest_ctx(test_db.store().pool(), storage_partition_id).await;

    let report = ingest_turn_direct_with_ctx(
        ctx,
        turn(
            storage_partition_id,
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
        node_valid_to(test_db.store().pool(), storage_partition_id, old_uid)
            .await
            .is_some()
    );
    assert_eq!(
        fact_rows(test_db.store().pool(), storage_partition_id)
            .await
            .len(),
        0
    );
    assert_eq!(
        user_fact_rows(test_db.store().pool(), storage_partition_id)
            .await
            .len(),
        2
    );
    let active_user_rows =
        active_user_fact_rows(test_db.store().pool(), storage_partition_id).await;
    assert_eq!(
        active_user_rows
            .iter()
            .map(|row| row.name.as_str())
            .collect::<Vec<_>>(),
        vec!["API"]
    );
    assert_eq!(
        active_tenant_fact_rows(test_db.store().pool(), storage_partition_id)
            .await
            .len(),
        0
    );
    assert!(
        supersedes_edge_exists(
            test_db.store().pool(),
            storage_partition_id,
            old_uid,
            active_user_rows[0].uid
        )
        .await
    );
    assert_eq!(
        supersede_protocol_count(test_db.store().pool(), storage_partition_id).await,
        1
    );
    assert_eq!(
        contradiction_edge_count(test_db.store().pool(), storage_partition_id).await,
        0
    );
}

#[tokio::test]
async fn slow_path_skips_chunks_that_yield_no_extractable_facts() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let storage_partition_id = Uuid::now_v7();
    let ctx = ingest_ctx(test_db.store().pool(), storage_partition_id).await;

    let report = ingest_turn_direct_with_ctx(
        ctx,
        turn(
            storage_partition_id,
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
    assert_eq!(
        active_tenant_fact_rows(test_db.store().pool(), storage_partition_id)
            .await
            .len(),
        0
    );
    let rows = active_user_fact_rows(test_db.store().pool(), storage_partition_id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "API");
}

#[tokio::test]
async fn slow_path_uses_scripted_extractor_for_fact_heuristic_would_skip() {
    // Pins: direct slow-path ingestion uses the configured FactExtractor seam before writes.
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let storage_partition_id = Uuid::now_v7();
    let transcript = "Should we retain the handoff note? Please review before standup.";
    let heuristic_probe = extract_facts(&[TurnChunk {
        index: 0,
        text: transcript.to_string(),
        token_estimate: 10,
    }])
    .expect("heuristic probe should extract");
    assert_eq!(heuristic_probe, Vec::new());
    let ctx = ingest_ctx(test_db.store().pool(), storage_partition_id)
        .await
        .with_extractor(Arc::new(
            ScriptedFactExtractor::from_summaries(["handoff note names standup owner"])
                .expect("scripted fixture should parse"),
        ));

    let report = ingest_turn_direct_with_ctx(ctx, turn(storage_partition_id, transcript, 7))
        .await
        .expect("slow path should ingest scripted extractor fact");

    assert_eq!(report.inserted, 1);
    assert_eq!(report.superseded, 0);
    assert_eq!(report.skipped, 0);
    assert_eq!(report.failed, 0);
    let rows = active_user_fact_rows(test_db.store().pool(), storage_partition_id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "handoff");
    let properties = rows[0]
        .properties_summary
        .as_ref()
        .expect("scripted fact properties projected");
    assert_eq!(properties["summary"], "handoff note names standup owner");
    assert_eq!(properties["predicate"], "note");
    assert_eq!(properties["object"], "names standup owner");
    assert_eq!(properties["source_chunk"], 0);
}

#[tokio::test]
async fn stated_event_time_backdates_fact_valid_from() {
    // Pins: an extractor-provided event time becomes the fact node's
    // `valid_from` so recency ranking and as-of reads reflect when the fact
    // became true, while a future-dated event time falls back to the turn
    // instant instead of creating a not-yet-valid fact.
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let pool = test_db.store().pool();
    let storage_partition_id = Uuid::now_v7();
    let stated = fixed_time() - Duration::days(120);
    let future = fixed_time() + Duration::days(5);
    let mut facts = extract_facts(&[
        TurnChunk {
            index: 0,
            text: "Fact: contact moved_to Denver".to_string(),
            token_estimate: 4,
        },
        TurnChunk {
            index: 1,
            text: "Fact: contact joined team_beta".to_string(),
            token_estimate: 4,
        },
    ])
    .expect("scripted facts extract");
    facts[0].event_time = Some(stated);
    facts[1].event_time = Some(future);
    let ctx = ingest_ctx(pool, storage_partition_id)
        .await
        .with_extractor(Arc::new(ScriptedFactExtractor::new(facts)));

    let report = ingest_turn_direct_with_ctx(ctx, turn(storage_partition_id, "transcript", 3))
        .await
        .expect("slow path ingests dated facts");

    assert_eq!(report.inserted, 2);
    let rows = active_user_fact_rows(pool, storage_partition_id).await;
    assert_eq!(rows.len(), 2);
    let by_object = |needle: &str| {
        rows.iter()
            .find(|row| {
                row.properties_summary
                    .as_ref()
                    .and_then(|properties| properties.get("object"))
                    .and_then(serde_json::Value::as_str)
                    == Some(needle)
            })
            .expect("expected fact present")
    };
    assert_eq!(
        by_object("Denver").valid_from,
        stated,
        "stated event time must backdate valid_from"
    );
    assert_eq!(
        by_object("team_beta").valid_from,
        fixed_time(),
        "future-dated event time must fall back to the turn instant"
    );
}

#[tokio::test]
async fn slow_path_resolves_entity_nodes_and_reuses_subject_across_sessions() {
    // Pins: slow-path ingestion links contact fact endpoints to contact-scoped Entity nodes.
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let storage_partition_id = Uuid::now_v7();
    let ctx = ingest_ctx(test_db.store().pool(), storage_partition_id)
        .await
        .with_entity_merge_verifier(Arc::new(DeterministicEntityMergeVerifier));

    let first = ingest_turn_direct_with_ctx(
        ctx.clone(),
        turn(storage_partition_id, "Fact: API runs_on_port 3000", 8),
    )
    .await
    .expect("first fact ingests with entities");
    let second =
        ingest_turn_direct_with_ctx(ctx, turn(storage_partition_id, "Fact: API uses Redis", 9))
            .await
            .expect("second fact reuses API entity");

    assert_eq!(first.inserted, 1);
    assert_eq!(second.inserted, 1);
    assert_eq!(first.failed + second.failed, 0);

    assert_eq!(
        entity_rows(test_db.store().pool(), storage_partition_id)
            .await
            .len(),
        0
    );
    let entities = active_user_entity_rows(test_db.store().pool(), storage_partition_id).await;
    let entity_names = entities
        .iter()
        .map(|row| row.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(entity_names, vec!["3000", "API", "Redis"]);
    let api_uid = entities
        .iter()
        .find(|row| row.name == "API")
        .expect("API entity exists")
        .uid
        .to_string();
    let entity_uids = entities
        .iter()
        .map(|row| row.uid.to_string())
        .collect::<HashSet<_>>();
    assert_eq!(entity_uids.len(), 3);

    let fact_uids = active_user_fact_rows(test_db.store().pool(), storage_partition_id)
        .await
        .into_iter()
        .map(|row| row.uid.to_string())
        .collect::<HashSet<_>>();
    assert_eq!(fact_uids.len(), 2);
    let relates_to_edges = relates_to_edges(test_db.store().pool(), storage_partition_id).await;
    assert_eq!(
        relates_to_edges
            .iter()
            .filter(|(start_uid, end_uid, role)| {
                role == "subject" && start_uid == &api_uid && fact_uids.contains(end_uid)
            })
            .count(),
        2
    );
    let all_entity_edges =
        entity_resolution_edges(test_db.store().pool(), storage_partition_id).await;
    assert_eq!(
        all_entity_edges
            .iter()
            .filter(|(_, start_uid, end_uid, role)| {
                role == "object" && fact_uids.contains(start_uid) && entity_uids.contains(end_uid)
            })
            .count(),
        2
    );
}

#[tokio::test]
async fn slow_path_multi_hop_facts_expand_through_shared_object_entity() {
    // Pins: corpus-style dependency and ownership facts are connected as Fact -> Entity -> Fact.
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let storage_partition_id = Uuid::now_v7();
    let ctx = ingest_ctx(test_db.store().pool(), storage_partition_id).await;
    let graph = ctx.graph.clone();

    let dependency = ingest_turn_direct_with_ctx(
        ctx.clone(),
        turn(
            storage_partition_id,
            "Fact: tenant shared audit-shipper-dep-test depends_on is lib-audit-wire-test.",
            10,
        ),
    )
    .await
    .expect("dependency fact ingests");
    let ownership = ingest_turn_direct_with_ctx(
        ctx,
        turn(
            storage_partition_id,
            "Fact: tenant shared lib-audit-wire-test owned_by is profile-experience-test.",
            11,
        ),
    )
    .await
    .expect("ownership fact ingests");

    assert_eq!(dependency.inserted, 1, "{dependency:?}");
    assert_eq!(ownership.inserted, 1, "{ownership:?}");

    let facts = active_tenant_fact_rows(test_db.store().pool(), storage_partition_id).await;
    let dependency_uid = fact_uid_with_subject(&facts, "audit-shipper-dep-test");
    let owner_uid = fact_uid_with_subject(&facts, "lib-audit-wire-test");

    let expanded = graph
        .expand_seeds(&[dependency_uid], 3, None, &GraphWalkScoring::default())
        .await
        .expect("expand dependency fact");
    let entities = active_tenant_entity_rows(test_db.store().pool(), storage_partition_id).await;
    let edges = relates_to_edges(test_db.store().pool(), storage_partition_id).await;
    let owner_hit = expanded
        .iter()
        .find(|hit| hit.uid == owner_uid)
        .unwrap_or_else(|| {
            panic!(
                "owner fact should be reachable through shared library entity; facts={facts:?} entities={entities:?} edges={edges:?} expanded={expanded:?}"
            )
        });

    assert_eq!(owner_hit.seed, dependency_uid);
    assert_eq!(owner_hit.label, NodeLabel::Fact);
    assert_eq!(owner_hit.hop, 2);
    assert_eq!(
        owner_hit.edges,
        vec![EdgeLabel::DependsOn, EdgeLabel::RelatesTo]
    );
}

#[tokio::test]
async fn slow_path_is_atomic_when_a_chunk_fails_partway_through_extraction() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let storage_partition_id = Uuid::now_v7();
    let ctx = ingest_ctx_with_pii(
        test_db.store().pool(),
        storage_partition_id,
        Arc::new(support::FailOnNthPiiClassifier::new(2)),
    )
    .await;

    let error = ingest_turn_direct_with_ctx(
        ctx,
        turn(
            storage_partition_id,
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
        fact_rows(test_db.store().pool(), storage_partition_id)
            .await
            .is_empty()
    );
    assert!(
        user_fact_rows(test_db.store().pool(), storage_partition_id)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn slow_path_abstained_pii_writes_no_plaintext_or_derived_rows_db_memory() {
    // Pins: fail-closed PII abstention aborts the production direct-ingestion path
    // before plaintext or any graph, changelog, vector, or dedup projection is durable.
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let storage_partition_id = Uuid::now_v7();
    let ctx = ingest_ctx_with_pii(
        test_db.store().pool(),
        storage_partition_id,
        Arc::new(support::FailClosedPiiClassifier::new(
            "privacy-filter:test-fail-closed",
        )),
    )
    .await;

    let error = ingest_turn_direct_with_ctx(
        ctx,
        turn(
            storage_partition_id,
            "Fact: alice@example.com owns secret sk-live",
            51,
        ),
    )
    .await
    .expect_err("fail-closed PII abstention should abort slow-path ingestion");

    let source = std::error::Error::source(error.as_ref() as &(dyn std::error::Error + 'static))
        .expect("retryable handler error should preserve the ingestion error source");
    assert!(matches!(
        source.downcast_ref::<IngestError>(),
        Some(IngestError::PiiClassificationUnavailable { model_version })
            if model_version == "privacy-filter:test-fail-closed"
    ));
    assert_eq!(
        durable_ingest_counts(test_db.store().pool(), storage_partition_id).await,
        DurableIngestCounts {
            graph_nodes: 0,
            graph_creates: 0,
            vectors: 0,
            dedup_rows: 0,
        },
        "abstaining PII classification must leave no durable memory side effects"
    );
}

#[tokio::test]
async fn slow_path_emits_lineage_events_for_each_extracted_fact() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let storage_partition_id = Uuid::now_v7();
    let ctx = ingest_ctx(test_db.store().pool(), storage_partition_id).await;
    let session_turn = turn(
        storage_partition_id,
        include_str!("support/fixtures/document_simple.md"),
        6,
    );
    let session_id = session_turn.session_id.to_string();

    let report = ingest_turn_direct_with_ctx(ctx, session_turn)
        .await
        .expect("slow path writes lineage changelog rows");

    assert_eq!(report.inserted, 3);
    let payloads = create_changelog_payloads(test_db.store().pool(), storage_partition_id).await;
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

fn fact_uid_with_subject(rows: &[moa_memory_graph::NodeIndexRow], subject: &str) -> Uuid {
    rows.iter()
        .find(|row| {
            row.properties_summary
                .as_ref()
                .and_then(|properties| properties.get("subject"))
                .and_then(serde_json::Value::as_str)
                == Some(subject)
        })
        .map(|row| row.uid)
        .expect("fact with expected subject should exist")
}

async fn durable_ingest_counts(pool: &PgPool, storage_partition_id: Uuid) -> DurableIngestCounts {
    DurableIngestCounts {
        graph_nodes: count_storage_partition_rows(pool, storage_partition_id, "moa.node_index")
            .await,
        graph_creates: count_graph_create_rows(pool, storage_partition_id).await,
        vectors: count_storage_partition_rows(pool, storage_partition_id, "moa.embeddings").await,
        dedup_rows: count_storage_partition_rows(pool, storage_partition_id, "moa.ingest_dedup")
            .await,
    }
}

async fn count_storage_partition_rows(
    pool: &PgPool,
    storage_partition_id: Uuid,
    table: &str,
) -> i64 {
    let mut conn = support::user_scoped_conn(pool, storage_partition_id).await;
    let query = format!("SELECT COUNT(*) FROM {table} WHERE storage_partition_id = $1");
    let count = sqlx::query_scalar::<_, i64>(&query)
        .bind(storage_partition_id.to_string())
        .fetch_one(conn.as_mut())
        .await
        .expect("count storage-partition rows");
    conn.commit()
        .await
        .expect("commit storage-partition row count");
    count
}

async fn count_graph_create_rows(pool: &PgPool, storage_partition_id: Uuid) -> i64 {
    let mut conn = support::user_scoped_conn(pool, storage_partition_id).await;
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM moa.graph_changelog \
         WHERE storage_partition_id = $1 AND op = 'create'",
    )
    .bind(storage_partition_id.to_string())
    .fetch_one(conn.as_mut())
    .await
    .expect("count graph create changelog rows");
    conn.commit()
        .await
        .expect("commit graph create changelog count");
    count
}

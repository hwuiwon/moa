//! Partition-layout invariants for the hash-partitioned `events` table.

use moa_core::{
    events::Event, traits::SessionStore, types::events_stream::EventRange,
    types::identifiers::TenantId,
};
use moa_test_support::fixtures::session_meta_fixture;
use moa_test_support::postgres::bootstrap_test_db;
use uuid::Uuid;

fn qualified(schema_name: &str, table_name: &str) -> String {
    format!(
        "\"{}\".\"{}\"",
        schema_name.replace('"', "\"\""),
        table_name.replace('"', "\"\"")
    )
}

async fn explain_plan(pool: &sqlx::PgPool, sql: &str) -> String {
    let rows: Vec<String> = sqlx::query_scalar(&format!("EXPLAIN {sql}"))
        .fetch_all(pool)
        .await
        .expect("EXPLAIN query plan");
    rows.join("\n")
}

/// Counts how many of the 16 `events_pNN` partitions appear in a query plan.
fn partitions_scanned(plan: &str) -> usize {
    (0..16)
        .filter(|index| plan.contains(&format!("events_p{index:02}")))
        .count()
}

#[tokio::test]
async fn events_session_filter_prunes_to_one_partition_while_cross_session_scan_fans_out_db() {
    // Pins: a session_id-filtered read prunes to exactly one of the 16 HASH
    // partitions (the point of partitioning the hot events table), while a
    // cross-session scan with no session_id predicate fans out across all of
    // them. The filtered read must also still round-trip the event content.
    let test_db = bootstrap_test_db().await.expect(
        "bootstrap Postgres test database; start the compose Postgres or set MOA_DATABASE_URL",
    );
    let session_id = test_db
        .store()
        .create_session(session_meta_fixture(TenantId::from(Uuid::now_v7())))
        .await
        .expect("create session for partition pruning check");
    test_db
        .store()
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "partition routing check".to_string(),
                attachments: Vec::new(),
            },
        )
        .await
        .expect("emit event into its partition");

    let events = qualified(test_db.schema_name(), "events");
    let pool = test_db.store().pool();

    let pruned = explain_plan(
        pool,
        &format!(
            "SELECT id FROM {events} WHERE session_id = '{}'::uuid",
            session_id.0
        ),
    )
    .await;
    assert_eq!(
        partitions_scanned(&pruned),
        1,
        "session_id-filtered read should prune to exactly one partition, plan:\n{pruned}"
    );

    let full = explain_plan(
        pool,
        &format!("SELECT id FROM {events} WHERE event_type = 'UserMessage'"),
    )
    .await;
    assert_eq!(
        partitions_scanned(&full),
        16,
        "cross-session scan without a session_id predicate should touch all 16 partitions, plan:\n{full}"
    );

    let replayed = test_db
        .store()
        .get_events(session_id, EventRange::all())
        .await
        .expect("replay event through its partition");
    assert_eq!(
        replayed.len(),
        1,
        "the single emitted event must round-trip"
    );
    assert_eq!(
        replayed[0].session_id, session_id,
        "replayed event must belong to the queried session"
    );
}

#[tokio::test]
async fn large_claim_checked_event_round_trips_through_partition_via_get_many_db() {
    // Pins: an event whose payload is offloaded behind a claim-check blob replays
    // byte-for-byte through get_events, which now resolves the referenced blob via
    // the batched BlobStore::get_many instead of a per-event get. Exercises the
    // non-empty-blob-list branch of the replay path against the partitioned table.
    let test_db = bootstrap_test_db().await.expect(
        "bootstrap Postgres test database; start the compose Postgres or set MOA_DATABASE_URL",
    );
    let session_id = test_db
        .store()
        .create_session(session_meta_fixture(TenantId::from(Uuid::now_v7())))
        .await
        .expect("create session for claim-check replay");

    // Comfortably exceed the 65_536-byte blob offload threshold so the payload's
    // large string is stored behind a claim check rather than inline.
    let big_text = "moa-partition-replay ".repeat(4096);
    assert!(
        big_text.len() > 65_536,
        "payload must exceed the blob offload threshold to be claim-checked"
    );
    test_db
        .store()
        .emit_event(
            session_id,
            Event::UserMessage {
                text: big_text.clone(),
                attachments: Vec::new(),
            },
        )
        .await
        .expect("emit claim-checked event");

    let replayed = test_db
        .store()
        .get_events(session_id, EventRange::all())
        .await
        .expect("replay claim-checked event via get_many");
    assert_eq!(
        replayed.len(),
        1,
        "the single emitted event must round-trip"
    );
    match &replayed[0].event {
        Event::UserMessage { text, .. } => assert_eq!(
            text, &big_text,
            "claim-checked payload must be resolved and round-trip fully"
        ),
        other => panic!("unexpected replayed event variant: {other:?}"),
    }
}

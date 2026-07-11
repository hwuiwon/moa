//! Unit tests for row routing, retry classification, journal acknowledgement, and compliance folding.

use super::compliance::{ExistingChainRows, fold_partition_chain};
use super::journal::should_ack_journal;
use super::retry::{
    WriteDisposition, dead_letter_summary, is_retryable_postgres_sqlstate,
    is_retryable_write_error, stable_dead_letter_id,
};
use super::rows::{LineageRow, PendingRow};
use crate::Error;

use chrono::Utc;
use moa_core::types::identifiers::StoragePartitionId;
use moa_lineage_core::chain::HashChain;
use moa_lineage_core::{LineageEvent, ScoreRecord, ScoreSource, ScoreTarget, ScoreValue, TurnId};
use uuid::Uuid;

fn test_lineage_row(partition: &str, payload: serde_json::Value) -> LineageRow {
    LineageRow {
        turn_id: Uuid::now_v7(),
        session_id: Uuid::now_v7(),
        user_id: "chain-user".to_string(),
        storage_partition_id: partition.to_string(),
        ts: Utc::now(),
        tier: 1,
        record_kind: 1,
        payload,
        integrity_hash: Vec::new(),
        prev_hash: None,
    }
}

#[test]
fn append_batch_groups_appends_into_one_persist() {
    // Pins: a batched append performs exactly one durability sync regardless of batch size
    // (group commit), while single appends sync per row.
    use crate::fjall_journal::Journal;

    let dir = std::env::temp_dir().join(format!("moa-lineage-groupcommit-{}", Uuid::now_v7()));
    let journal = Journal::open(&dir).expect("journal should open");

    let baseline = journal.persist_count();
    let entries: Vec<(u64, Vec<u8>)> = (1..=5).map(|seq| (seq, vec![seq as u8; 8])).collect();
    journal
        .append_batch(&entries)
        .expect("batch append should sync");
    assert_eq!(
        journal.persist_count() - baseline,
        1,
        "a five-row batch append must sync exactly once"
    );
    assert_eq!(journal.approximate_len(), 5);

    let before_single = journal.persist_count();
    journal
        .append(6, b"one")
        .expect("single append should sync");
    journal
        .append(7, b"two")
        .expect("single append should sync");
    assert_eq!(
        journal.persist_count() - before_single,
        2,
        "single appends must sync once per row"
    );
}

#[test]
fn fold_partition_chain_matches_sequential_link_walk() {
    // Pins: the batched in-memory fold yields the same per-row integrity/prev hashes and the
    // same final tip as a straight per-row HashChain::link walk from the same starting tip.
    let payloads: Vec<serde_json::Value> = (0..4)
        .map(|n| serde_json::json!({ "event": "e", "n": n }))
        .collect();
    let partition = "chain-partition";
    let mut rows: Vec<LineageRow> = payloads
        .iter()
        .map(|payload| test_lineage_row(partition, payload.clone()))
        .collect();
    let indices: Vec<usize> = (0..rows.len()).collect();
    let existing = ExistingChainRows::new();

    let outcome =
        fold_partition_chain(None, &mut rows, &indices, &existing).expect("fold should succeed");
    assert_eq!(outcome.new_rows, 4);

    let mut prev = None;
    let mut expected_final = None;
    for (row, payload) in rows.iter().zip(&payloads) {
        let (integrity, prev_echo) = HashChain::link(prev, payload).expect("link");
        assert_eq!(row.integrity_hash, integrity.as_bytes().to_vec());
        assert_eq!(
            row.prev_hash,
            prev_echo.map(|hash| hash.as_bytes().to_vec())
        );
        prev = Some(integrity);
        expected_final = Some(integrity.as_bytes().to_vec());
    }
    assert_eq!(outcome.final_hash, expected_final);
    assert_eq!(outcome.last_ts, Some(rows[3].ts));
}

#[test]
fn fold_partition_chain_reuses_existing_rows_without_advancing() {
    // Pins: a row already present in turn_lineage reuses its stored hashes and does not
    // advance the chain tip, so replayed batches are idempotent.
    let payload_a = serde_json::json!({ "event": "a" });
    let payload_b = serde_json::json!({ "event": "b" });
    let partition = "chain-partition";
    let mut rows = vec![
        test_lineage_row(partition, payload_a),
        test_lineage_row(partition, payload_b.clone()),
    ];
    let indices = vec![0_usize, 1];

    let mut existing = ExistingChainRows::new();
    existing.insert(
        (rows[0].turn_id, rows[0].record_kind, rows[0].ts),
        (vec![9_u8; 32], Some(vec![8_u8; 32])),
    );

    let outcome =
        fold_partition_chain(None, &mut rows, &indices, &existing).expect("fold should succeed");

    assert_eq!(rows[0].integrity_hash, vec![9_u8; 32]);
    assert_eq!(rows[0].prev_hash, Some(vec![8_u8; 32]));
    assert_eq!(outcome.new_rows, 1);
    let (expected, _) = HashChain::link(None, &payload_b).expect("link");
    assert_eq!(rows[1].integrity_hash, expected.as_bytes().to_vec());
    assert_eq!(outcome.final_hash, Some(expected.as_bytes().to_vec()));
}

#[test]
fn pending_row_routes_eval_events_to_scores() {
    let score_id = Uuid::now_v7();
    let row = PendingRow::from_event(LineageEvent::Eval(ScoreRecord {
        score_id,
        ts: Utc::now(),
        target: ScoreTarget::Turn {
            turn_id: TurnId::new_v7(),
        },
        storage_partition_id: StoragePartitionId::new("tenant"),
        user_id: None,
        name: "retrieval_zero_recall".to_string(),
        value: ScoreValue::Boolean(false),
        source: ScoreSource::OnlineJudge,
        model_or_evaluator: "retriever".to_string(),
        run_id: None,
        dataset_id: None,
        comment: None,
    }))
    .expect("score row should build");

    match row {
        PendingRow::Score(row) => {
            assert_eq!(row.score_id, score_id);
            assert_eq!(row.name, "retrieval_zero_recall");
            assert_eq!(row.value_type, "boolean");
            assert_eq!(row.value_boolean, Some(false));
        }
        PendingRow::Lineage(_) => panic!("eval events must not enter turn_lineage"),
    }
}

#[test]
fn write_retry_classification_is_sqlstate_aware() {
    // Pins: lineage writer retries transient database failures but not
    // permanent ones. The attempt count is bounded structurally by the
    // backon `with_max_times(WRITE_RETRY_MAX_ATTEMPTS - 1)` budget.
    assert!(is_retryable_postgres_sqlstate("08006"));
    assert!(is_retryable_postgres_sqlstate("40001"));
    assert!(!is_retryable_postgres_sqlstate("23505"));

    let permanent = Error::Invalid("poison row".to_string());
    assert!(!is_retryable_write_error(&permanent));

    let transient = Error::Sqlx(sqlx::Error::PoolTimedOut);
    assert!(is_retryable_write_error(&transient));
}

#[test]
fn dead_letter_summary_uses_first_row_metadata() {
    // Pins: dead-letter records carry searchable metadata from the batch head.
    let turn_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();
    let row = PendingRow::Lineage(LineageRow {
        turn_id,
        session_id,
        user_id: "user-1".to_string(),
        storage_partition_id: "partition-1".to_string(),
        ts: Utc::now(),
        tier: 1,
        record_kind: 1,
        payload: serde_json::json!({"kind": "test"}),
        integrity_hash: vec![7; 32],
        prev_hash: None,
    });

    let summary = dead_letter_summary(&[row]);

    assert_eq!(summary.row_count, 1);
    assert_eq!(
        summary.first_storage_partition_id.as_deref(),
        Some("partition-1")
    );
    assert_eq!(summary.first_session_id, Some(session_id));
    assert_eq!(summary.first_turn_id, Some(turn_id));
}

#[test]
fn dead_letter_disposition_acks_journal() {
    // Pins (F16): both successful writes and dead-letters acknowledge the journal
    // sequences. Dead-lettered rows are durably committed to the DLQ table first,
    // so acking removes them from the journal (bounding local storage) instead of
    // retaining and re-dead-lettering them on every restart.
    assert!(should_ack_journal(WriteDisposition::Written));
    assert!(should_ack_journal(WriteDisposition::DeadLettered));
}

#[test]
fn stable_dead_letter_id_dedupes_replayed_poison_batch() {
    // Pins: leaving a poison batch pending does not create unbounded duplicate dead letters.
    let row = PendingRow::Lineage(LineageRow {
        turn_id: Uuid::now_v7(),
        session_id: Uuid::now_v7(),
        user_id: "user-1".to_string(),
        storage_partition_id: "partition-1".to_string(),
        ts: Utc::now(),
        tier: 1,
        record_kind: 1,
        payload: serde_json::json!({"kind": "test"}),
        integrity_hash: vec![7; 32],
        prev_hash: None,
    });

    let first =
        stable_dead_letter_id(std::slice::from_ref(&row)).expect("dead-letter id should compute");
    let second = stable_dead_letter_id(&[row]).expect("dead-letter id should compute");

    assert_eq!(first, second);
}

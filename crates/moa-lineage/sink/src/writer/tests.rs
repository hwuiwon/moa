//! Unit tests for row routing, failure classification, and compliance folding.

use std::time::Duration;

use super::compliance::{ExistingChainRows, fold_partition_chain};
use super::retry::{
    FailureDisposition, MAX_CLAIM_ATTEMPTS, classify_failure, dead_letter_summary,
    is_retryable_postgres_sqlstate, is_retryable_write_error, stable_dead_letter_id,
};
use super::rows::{LineageRow, PendingRow, decode_pending_row, pending_row_event_class};
use super::{SharedWriterState, WriterState, WriterStats};
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
        experiment_provenance: None,
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
    // permanent ones.
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

#[test]
fn a_recoverable_failure_defers_and_a_permanent_one_dead_letters() {
    // Pins: the disposition of a failed batch is decided by what the error IS,
    // not by how many times it has been seen. A transient database failure
    // preserves the rows; a payload that can never be written consumes them.
    // Dead-lettering a retryable failure would destroy accepted records the
    // moment Postgres blipped.
    let transient = Error::Sqlx(sqlx::Error::PoolTimedOut);
    let permanent = Error::Invalid("undecodable payload".to_string());

    let transient_disposition = classify_failure(&transient, 1);
    assert!(
        matches!(transient_disposition, FailureDisposition::Retry { .. }),
        "a pool timeout must preserve the rows for retry, got {transient_disposition:?}"
    );
    assert_eq!(
        classify_failure(&permanent, 1),
        FailureDisposition::Permanent,
        "an error no retry can fix must dead-letter on the first attempt rather than \
         cycling the row through the queue until its attempt budget runs out"
    );
}

#[test]
fn a_retryable_failure_becomes_permanent_at_the_attempt_ceiling() {
    // Pins: the retry budget terminates. Without a ceiling, a row that fails a
    // retryable-looking check forever sits at the head of the claim index and
    // every row accepted behind it waits on it.
    let transient = Error::Sqlx(sqlx::Error::PoolTimedOut);

    let below = classify_failure(&transient, MAX_CLAIM_ATTEMPTS - 1);
    assert!(
        matches!(below, FailureDisposition::Retry { .. }),
        "attempt {} is still inside the budget, got {below:?}",
        MAX_CLAIM_ATTEMPTS - 1
    );
    assert_eq!(
        classify_failure(&transient, MAX_CLAIM_ATTEMPTS),
        FailureDisposition::Permanent,
        "attempt {MAX_CLAIM_ATTEMPTS} exhausts the budget and must dead-letter"
    );
}

#[test]
fn retry_backoff_grows_and_is_capped() {
    // Pins: backoff widens with attempts so a sustained outage is not re-probed
    // at full rate, and is capped so recovery is noticed promptly rather than
    // after an exponentially long sleep.
    let transient = Error::Sqlx(sqlx::Error::PoolTimedOut);
    let backoff_at = |attempts| match classify_failure(&transient, attempts) {
        FailureDisposition::Retry { backoff } => backoff,
        FailureDisposition::Permanent => {
            panic!("attempt {attempts} must still be retryable for this test to mean anything")
        }
    };

    let first = backoff_at(1);
    let later = backoff_at(4);
    assert!(
        later > first,
        "backoff must grow with attempts: attempt 1 gave {first:?}, attempt 4 gave {later:?}"
    );
    assert!(
        later <= Duration::from_secs(30),
        "backoff must stay capped so recovery is re-probed promptly, got {later:?}"
    );
}

#[test]
fn event_class_matches_the_row_shape_it_labels() {
    // Pins: the queue's `event_class` column and the payload it labels are
    // derived from the same value. A label that disagreed with its payload would
    // route a score row through lineage backlog metrics and dead-letter
    // accounting while the database CHECK stayed happy.
    let lineage = PendingRow::Lineage(test_lineage_row(
        "class-partition",
        serde_json::json!({"kind": "test"}),
    ));
    let score = PendingRow::from_event(eval_event()).expect("eval event should render a row");

    assert_eq!(pending_row_event_class(&lineage), "lineage");
    assert_eq!(pending_row_event_class(&score), "score");
}

#[test]
fn a_committed_payload_round_trips_through_the_queue_encoding() {
    // Pins: what `accept_batch` serializes is what the drain decodes. A drift
    // here would strand every already-accepted row as an undecodable payload,
    // dead-lettering committed records the caller was told were safe.
    let original = PendingRow::Lineage(test_lineage_row(
        "round-trip-partition",
        serde_json::json!({"kind": "round-trip"}),
    ));
    let encoded = serde_json::to_value(&original).expect("pending row should serialize");

    let decoded = decode_pending_row(encoded).expect("the queue encoding must decode back");

    assert_eq!(
        decoded.storage_partition_id(),
        "round-trip-partition",
        "decoded row lost its purge scope"
    );
    assert_eq!(pending_row_event_class(&decoded), "lineage");
}

#[test]
fn writer_states_have_distinct_stable_labels() {
    // Pins: the readiness probe and the state gauge both key off these labels,
    // so two states sharing one label would make a failed writer indistinguishable
    // from a running one in every dashboard and alert built on it.
    let labels = [
        WriterState::Running.as_str(),
        WriterState::Draining.as_str(),
        WriterState::Failed.as_str(),
        WriterState::Stopped.as_str(),
    ];
    let unique = labels.iter().collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        unique.len(),
        labels.len(),
        "writer state labels must be distinct, got {labels:?}"
    );
}

#[test]
fn default_stats_report_no_work_rather_than_a_fake_flush() {
    // Pins: a writer that has never flushed reports `None`, not epoch zero. A
    // zero timestamp would read as "flushed in 1970" to any staleness alert.
    let stats = WriterStats::default();

    assert_eq!(stats.written, 0);
    assert_eq!(stats.pending, 0);
    assert_eq!(stats.last_flush_unix_ms, None);
}

/// Builds one `LineageEvent::Eval`, the only event that becomes a score row.
fn eval_event() -> LineageEvent {
    LineageEvent::Eval(ScoreRecord {
        score_id: Uuid::now_v7(),
        ts: Utc::now(),
        target: ScoreTarget::Turn {
            turn_id: TurnId::new_v7(),
        },
        storage_partition_id: StoragePartitionId::new("class-partition"),
        user_id: None,
        name: "helpfulness".to_string(),
        value: ScoreValue::Boolean(false),
        source: ScoreSource::ProductEvaluator,
        model_or_evaluator: "unit-test@v1".to_string(),
        run_id: None,
        dataset_id: None,
        comment: None,
        experiment_provenance: None,
    })
}

#[test]
fn a_state_transition_zeroes_the_state_it_left() {
    // Pins the shape an alert actually reads. `moa_lineage_writer_state` is a
    // labelled gauge, and setting only the new label to 1.0 leaves the previous
    // one at 1.0 forever - so after Running -> Failed -> Running, a
    // `state="failed"` alert stays firing until the process restarts. An alert
    // that cannot clear is worse than no alert: it gets muted, and then it
    // guards nothing.
    //
    // Rendered through a real Prometheus recorder rather than a stand-in,
    // because a stand-in that records only the last write cannot show a latched
    // series at all - which is precisely the failure under test.
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();

    metrics::with_local_recorder(&recorder, || {
        let shared = SharedWriterState::new();
        shared.set_state(WriterState::Running);
        shared.set_state(WriterState::Failed);
        shared.set_state(WriterState::Running);
    });

    let rendered = handle.render();
    let series = rendered
        .lines()
        .filter(|line| line.starts_with("moa_lineage_writer_state{"))
        .collect::<Vec<_>>();
    let firing = series
        .iter()
        .filter(|line| line.ends_with(" 1"))
        .collect::<Vec<_>>();

    assert_eq!(
        firing.len(),
        1,
        "exactly one writer state may read 1 at a time; rendered series were:\n{}",
        series.join("\n")
    );
    assert!(
        firing[0].contains("state=\"running\""),
        "the surviving series must be the state actually reached, got: {}",
        firing[0]
    );
    assert!(
        series
            .iter()
            .any(|line| line.contains("state=\"failed\"") && line.ends_with(" 0")),
        "the state that was left must be explicitly zeroed, not merely absent, or an \
         alert already scraping it never clears; rendered series were:\n{}",
        series.join("\n")
    );
}

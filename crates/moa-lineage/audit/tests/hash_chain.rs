//! Adversarial out-of-line tests for audit hash-chain verification.

#[path = "support/hash_chain.rs"]
mod support;

use moa_lineage_core::chain::{LineageChainError, genesis_hash};
use support::{
    external_tip_hash, minimal_chain_records, push_wrong_prev_hash_record, verify_chain,
};

#[test]
fn hash_chain_verifies_intact_chain_of_5_records_returns_ok() {
    let records = minimal_chain_records();

    let tip = verify_chain(&records).expect("intact five-record chain should verify");

    assert_eq!(
        tip.as_bytes().as_slice(),
        records[4].integrity_hash.as_slice()
    );
    assert_eq!(
        records[0].prev_hash.as_deref(),
        Some(genesis_hash().as_bytes().as_slice())
    );
}

/// Tampering: modifies `records[2].payload.payload.input`; verifier must fail at index 2.
#[test]
fn hash_chain_verify_fails_when_middle_record_payload_is_modified() {
    let mut records = minimal_chain_records();
    records[2].payload["payload"]["input"] = serde_json::json!("cargo test --tampered");

    let error = verify_chain(&records).expect_err("tampered payload should break the chain");

    assert_chain_mismatch_at(error, 2);
}

/// Tampering: swaps records 2 and 3; verifier must fail at the first swapped index, 2.
#[test]
fn hash_chain_verify_fails_when_record_is_inserted_out_of_order() {
    let mut records = minimal_chain_records();
    records.swap(2, 3);

    let error = verify_chain(&records).expect_err("out-of-order records should break the chain");

    assert_chain_mismatch_at(error, 2);
}

/// Tampering: deletes record 2; verifier must fail at the new record occupying index 2.
#[test]
fn hash_chain_verify_fails_when_record_is_deleted() {
    let mut records = minimal_chain_records();
    records.remove(2);

    let error = verify_chain(&records).expect_err("deleted record should break the chain");

    assert_chain_mismatch_at(error, 2);
}

/// Tampering: appends record 5 with an integrity hash linked to genesis instead of record 4.
#[test]
fn hash_chain_verify_fails_when_record_is_appended_with_wrong_prev_hash() {
    let mut records = minimal_chain_records();
    push_wrong_prev_hash_record(&mut records);

    let error = verify_chain(&records).expect_err("wrong previous hash should break the chain");

    assert_chain_mismatch_at(error, 5);
}

#[test]
fn hash_chain_replay_against_external_witness_matches() {
    let records = minimal_chain_records();
    let verifier_tip = verify_chain(&records).expect("intact chain should verify");
    let witness_tip = external_tip_hash(&records).expect("external witness should compute tip");

    assert_eq!(verifier_tip, witness_tip);
}

fn assert_chain_mismatch_at(error: LineageChainError, expected_index: usize) {
    match error {
        LineageChainError::ChainMismatch { index, message } => {
            assert_eq!(index, expected_index);
            assert!(
                message.contains("stored integrity hash"),
                "unexpected chain mismatch message: {message}"
            );
        }
        other => panic!("expected ChainMismatch at {expected_index}, got {other:?}"),
    }
}

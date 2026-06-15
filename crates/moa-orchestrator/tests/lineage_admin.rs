//! LineageAdmin helper coverage.

use chrono::Utc;
use moa_lineage_audit::blake3_merkle_root;
use moa_lineage_core::chain::HashChain;
use moa_orchestrator::services::lineage_admin::{
    ComplianceRow, prepare_lineage_sql, verify_compliance_rows,
};
use serde_json::json;
use uuid::Uuid;

#[test]
fn prepare_lineage_sql_scopes_logical_source_to_workspace_and_since() {
    // Pins: LineageAdmin query rewrites the logical source to a workspace-scoped hot-store subquery.
    let sql = prepare_lineage_sql("SELECT count(*) FROM lineage WHERE record_kind = 4")
        .expect("lineage query should prepare");

    assert!(sql.contains("analytics.turn_lineage"));
    assert!(sql.contains("workspace_id = $1"));
    assert!(sql.contains("($2::text)::interval"));
    assert!(sql.contains("record_kind = 4"));
}

#[test]
fn prepare_lineage_sql_rejects_mutating_statement() {
    // Pins: LineageAdmin rejects mutating SQL before any database query runs.
    let error =
        prepare_lineage_sql("DELETE FROM lineage").expect_err("mutating lineage query should fail");

    assert!(format!("{error:?}").contains("only SELECT"));
}

#[test]
fn verify_compliance_rows_pins_chain_and_merkle_root() {
    // Pins: LineageAdmin verifies exact hash-chain links and the published Merkle root.
    let first_payload = json!({"record": {"kind": "first"}});
    let second_payload = json!({"record": {"kind": "second"}});
    let (first_hash, first_prev) =
        HashChain::link(None, &first_payload).expect("first hash should compute");
    let (second_hash, second_prev) =
        HashChain::link(Some(first_hash), &second_payload).expect("second hash should compute");
    let leaves = vec![
        first_hash.as_bytes().to_vec(),
        second_hash.as_bytes().to_vec(),
    ];
    let root = blake3_merkle_root(&leaves).expect("root should compute");
    let rows = vec![
        ComplianceRow {
            turn_id: Uuid::new_v4(),
            record_kind: 1,
            ts: Utc::now(),
            payload: first_payload,
            integrity_hash: first_hash.as_bytes().to_vec(),
            prev_hash: first_prev.map(|hash| hash.as_bytes().to_vec()),
        },
        ComplianceRow {
            turn_id: Uuid::new_v4(),
            record_kind: 2,
            ts: Utc::now(),
            payload: second_payload,
            integrity_hash: second_hash.as_bytes().to_vec(),
            prev_hash: second_prev.map(|hash| hash.as_bytes().to_vec()),
        },
    ];

    let report = verify_compliance_rows(rows, Some(root.as_bytes().to_vec()))
        .expect("valid chain should verify");

    assert_eq!(report.records, 2);
}

#[test]
fn verify_compliance_rows_rejects_broken_chain_link() {
    // Pins: LineageAdmin detects a stored prev_hash that does not match the previous row.
    let first_payload = json!({"record": {"kind": "first"}});
    let second_payload = json!({"record": {"kind": "second"}});
    let (first_hash, first_prev) =
        HashChain::link(None, &first_payload).expect("first hash should compute");
    let (second_hash, _) =
        HashChain::link(Some(first_hash), &second_payload).expect("second hash should compute");
    let rows = vec![
        ComplianceRow {
            turn_id: Uuid::new_v4(),
            record_kind: 1,
            ts: Utc::now(),
            payload: first_payload,
            integrity_hash: first_hash.as_bytes().to_vec(),
            prev_hash: first_prev.map(|hash| hash.as_bytes().to_vec()),
        },
        ComplianceRow {
            turn_id: Uuid::new_v4(),
            record_kind: 2,
            ts: Utc::now(),
            payload: second_payload,
            integrity_hash: second_hash.as_bytes().to_vec(),
            prev_hash: Some([9_u8; 32].to_vec()),
        },
    ];

    let error =
        verify_compliance_rows(rows, None).expect_err("broken chain link should fail verification");

    assert!(format!("{error:?}").contains("chain link mismatch"));
}

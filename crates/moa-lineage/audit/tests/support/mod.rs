//! Shared fixtures and assertions for lineage audit integration tests.

#![allow(dead_code)]

use std::path::PathBuf;

use blake3::Hash;
use moa_lineage_core::chain::{
    HashChain, Result, canonical_json_bytes, canonical_payload_hash, genesis_hash, next_chain_hash,
};
use serde_json::Value;

/// One test audit record with the stored hashes needed by hash-chain tests.
#[derive(Clone, Debug)]
pub(crate) struct ChainRecord {
    /// Canonical record payload.
    pub(crate) payload: Value,
    /// Stored chain integrity hash.
    pub(crate) integrity_hash: Vec<u8>,
    /// Stored previous hash, included for adversarial fixture readability.
    pub(crate) prev_hash: Option<Vec<u8>>,
}

/// Loads a JSON fixture by name.
pub(crate) fn fixture_json(name: &str) -> Value {
    serde_json::from_str(&fixture_text(name))
        .unwrap_or_else(|error| panic!("failed to parse fixture {name}: {error}"))
}

/// Loads a JSONL fixture into one value per non-empty line.
pub(crate) fn fixture_jsonl(name: &str) -> Vec<Value> {
    fixture_text(name)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("failed to parse fixture {name} line: {error}"))
        })
        .collect()
}

/// Builds the minimal five-record hash-chain fixture.
pub(crate) fn minimal_chain_records() -> Vec<ChainRecord> {
    build_chain(fixture_jsonl("audit_records_minimal.jsonl"))
        .expect("minimal hash-chain fixture should build")
}

/// Builds a chain from canonical payloads using the production append primitive.
pub(crate) fn build_chain(records: Vec<Value>) -> Result<Vec<ChainRecord>> {
    let mut prev = None;
    let mut out = Vec::with_capacity(records.len());
    for payload in records {
        let (integrity, stored_prev) = HashChain::link(prev, &payload)?;
        out.push(ChainRecord {
            payload,
            integrity_hash: integrity.as_bytes().to_vec(),
            prev_hash: stored_prev.map(|hash| hash.as_bytes().to_vec()),
        });
        prev = Some(integrity);
    }
    Ok(out)
}

/// Verifies a chain fixture with the production verifier.
pub(crate) fn verify_chain(records: &[ChainRecord]) -> Result<Hash> {
    HashChain::verify(
        records
            .iter()
            .map(|record| (&record.payload, record.integrity_hash.as_slice())),
    )
}

/// Computes the final chain hash without calling `HashChain::link` or `HashChain::verify`.
pub(crate) fn external_tip_hash(records: &[ChainRecord]) -> Result<Hash> {
    let mut prev = genesis_hash();
    for record in records {
        let canonical = canonical_json_bytes(&record.payload)?;
        let payload_hash = blake3::hash(&canonical);
        let mut hasher = blake3::Hasher::new();
        hasher.update(prev.as_bytes());
        hasher.update(payload_hash.as_bytes());
        prev = hasher.finalize();
    }
    Ok(prev)
}

/// Appends a sixth record whose stored integrity hash was computed from the wrong previous hash.
pub(crate) fn push_wrong_prev_hash_record(records: &mut Vec<ChainRecord>) {
    let payload = serde_json::json!({
        "record_id": "rec-006",
        "timestamp": "2026-05-07T12:00:05Z",
        "event_type": "assistant_response",
        "user_id": "user-001",
        "payload": {"message": "wrong prev", "sequence": 5}
    });
    let wrong_prev = genesis_hash();
    let payload_hash =
        canonical_payload_hash(&payload).expect("wrong-prev payload should hash canonically");
    let integrity = next_chain_hash(wrong_prev, payload_hash);
    records.push(ChainRecord {
        payload,
        integrity_hash: integrity.as_bytes().to_vec(),
        prev_hash: Some(wrong_prev.as_bytes().to_vec()),
    });
}

/// Loads a UTF-8 fixture as text.
pub(crate) fn fixture_text(name: &str) -> String {
    std::fs::read_to_string(fixture_path(name))
        .unwrap_or_else(|error| panic!("failed to read fixture {name}: {error}"))
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("support")
        .join("fixtures")
        .join(name)
}

//! BLAKE3 canonical-payload hash-chain primitives.

use blake3::{Hash, Hasher};
use serde::Serialize;
use serde_canonical_json::CanonicalFormatter;

use crate::error::{AuditError, Result};

const GENESIS_DOMAIN: &[u8] = b"\0\0\0\0moa-audit-genesis-v1\0\0\0\0";

/// Canonicalizes a serializable payload to deterministic JSON bytes.
pub fn canonical_json_bytes<T: Serialize>(payload: &T) -> Result<Vec<u8>> {
    let mut serializer =
        serde_json::Serializer::with_formatter(Vec::new(), CanonicalFormatter::new());
    payload.serialize(&mut serializer)?;
    Ok(serializer.into_inner())
}

/// Computes the BLAKE3 hash of canonical JSON payload bytes.
pub fn canonical_payload_hash(payload: &serde_json::Value) -> Result<Hash> {
    let canonical = canonical_json_bytes(payload)?;
    Ok(blake3::hash(&canonical))
}

/// Returns the deterministic workspace-chain genesis hash.
#[must_use]
pub fn genesis_hash() -> Hash {
    blake3::hash(GENESIS_DOMAIN)
}

/// Computes the next workspace-local chain hash from a previous hash.
#[must_use]
pub fn next_chain_hash(prev: Hash, payload_hash: Hash) -> Hash {
    let mut hasher = Hasher::new();
    hasher.update(prev.as_bytes());
    hasher.update(payload_hash.as_bytes());
    hasher.finalize()
}

/// BLAKE3 hash-chain helper.
pub struct HashChain;

impl HashChain {
    /// Returns `(integrity_hash, prev_hash)` for a compliance-tier payload.
    pub fn link(prev: Option<Hash>, payload: &serde_json::Value) -> Result<(Hash, Option<Hash>)> {
        let payload_hash = canonical_payload_hash(payload)?;
        let prev = prev.unwrap_or_else(genesis_hash);
        Ok((next_chain_hash(prev, payload_hash), Some(prev)))
    }

    /// Verifies a contiguous chain of canonical payloads and stored integrity hashes.
    pub fn verify<'a>(
        records: impl IntoIterator<Item = (&'a serde_json::Value, &'a [u8])>,
    ) -> Result<Hash> {
        let mut prev = None;
        for (index, (payload, expected)) in records.into_iter().enumerate() {
            let (actual, _) = Self::link(prev, payload)?;
            if actual.as_bytes() != expected {
                return Err(AuditError::ChainMismatch {
                    index,
                    message: "stored integrity hash did not match canonical payload".to_string(),
                });
            }
            prev = Some(actual);
        }
        Ok(prev.unwrap_or_else(genesis_hash))
    }
}

/// Converts a 32-byte slice into a BLAKE3 hash.
pub fn hash_from_slice(bytes: &[u8]) -> Result<Hash> {
    let array: [u8; 32] = bytes
        .try_into()
        .map_err(|_| AuditError::Invalid("expected a 32-byte hash".to_string()))?;
    Ok(Hash::from(array))
}

#[cfg(test)]
mod tests {
    use super::{HashChain, canonical_payload_hash};

    #[test]
    fn canonical_hash_is_key_order_stable() {
        let left = serde_json::json!({"b": 2, "a": 1});
        let right = serde_json::json!({"a": 1, "b": 2});

        assert_eq!(
            canonical_payload_hash(&left).expect("hash left"),
            canonical_payload_hash(&right).expect("hash right")
        );
    }

    #[test]
    fn chain_detects_tampered_payload() {
        let first = serde_json::json!({"event": "one"});
        let second = serde_json::json!({"event": "two"});
        let (first_hash, _) = HashChain::link(None, &first).expect("first link");
        let (second_hash, _) = HashChain::link(Some(first_hash), &second).expect("second link");

        let tampered = serde_json::json!({"event": "TWO"});
        let result = HashChain::verify([
            (&first, first_hash.as_bytes().as_slice()),
            (&tampered, second_hash.as_bytes().as_slice()),
        ]);

        assert!(result.is_err());
    }
}

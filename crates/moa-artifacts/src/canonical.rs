//! Canonical JSON serialization and SHA-256 hashing.

use serde::Serialize;
use serde_canonical_json::CanonicalFormatter;
use sha2::{Digest, Sha256};

use crate::Result;

/// Serializes a value with deterministic JSON object-key ordering.
pub fn canonical_json_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut serializer =
        serde_json::Serializer::with_formatter(Vec::new(), CanonicalFormatter::new());
    value.serialize(&mut serializer)?;
    Ok(serializer.into_inner())
}

/// Returns the SHA-256 digest of a value's canonical JSON bytes.
pub fn canonical_hash<T: Serialize>(value: &T) -> Result<[u8; 32]> {
    let bytes = canonical_json_bytes(value)?;
    Ok(Sha256::digest(bytes).into())
}

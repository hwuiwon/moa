//! SHA-256 hashing over MOA's shared canonical JSON byte contract.

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::Result;

/// Returns the SHA-256 digest of a value's canonical JSON bytes.
pub fn canonical_hash<T: Serialize>(value: &T) -> Result<[u8; 32]> {
    let bytes = moa_core::canonical_json::canonical_json_bytes(value)?;
    Ok(Sha256::digest(bytes).into())
}

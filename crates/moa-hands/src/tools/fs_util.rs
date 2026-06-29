//! Shared filesystem helpers for tool implementations.

use std::io::ErrorKind;
use std::path::Path;

use moa_core::Result;
use tokio::fs;

/// Reads a file's raw bytes, returning `Ok(None)` when the file does not exist.
///
/// Any other I/O failure propagates as an error. Callers decide how to interpret
/// the bytes (e.g. lossy text or binary classification), keeping their own UTF-8
/// strictness.
pub(crate) async fn read_optional_file_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path).await {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

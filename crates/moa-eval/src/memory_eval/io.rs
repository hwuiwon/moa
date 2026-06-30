//! Shared JSONL and config-validation I/O helpers for the memory eval modules.
//!
//! These were previously copied verbatim across the corpus, embeddings, gold,
//! generator, judge, and runner modules; this module is the single owner.

use std::path::Path;

use moa_eval_core::{EvalError, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Reads newline-delimited JSON records, skipping blank lines.
pub(crate) async fn read_jsonl<T>(path: &Path) -> Result<Vec<T>>
where
    T: DeserializeOwned,
{
    let file = File::open(path)
        .await
        .map_err(|source| io_error(path, source))?;
    let mut lines = BufReader::new(file).lines();
    let mut records = Vec::new();
    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|source| io_error(path, source))?
    {
        if line.trim().is_empty() {
            continue;
        }
        records.push(serde_json::from_str(&line)?);
    }
    Ok(records)
}

/// Writes records as newline-delimited JSON, creating parent directories.
pub(crate) async fn write_jsonl<T>(path: &Path, records: &[T]) -> Result<()>
where
    T: Serialize,
{
    ensure_parent_dir(path).await?;
    let mut file = File::create(path)
        .await
        .map_err(|source| io_error(path, source))?;
    for record in records {
        let line = serde_json::to_vec(record)?;
        file.write_all(&line)
            .await
            .map_err(|source| io_error(path, source))?;
        file.write_all(b"\n")
            .await
            .map_err(|source| io_error(path, source))?;
    }
    file.flush().await.map_err(|source| io_error(path, source))
}

/// Creates the parent directory of `path` when it has a non-empty parent.
pub(crate) async fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|source| io_error(parent, source))?;
    }
    Ok(())
}

/// Validates that a labelled config value is non-empty after trimming.
pub(crate) fn ensure_non_empty(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return invalid_config(format!("{label} must not be empty"));
    }
    Ok(())
}

/// Returns an `Err` invalid-config result for the provided message.
pub(crate) fn invalid_config<T>(message: impl Into<String>) -> Result<T> {
    Err(invalid_config_error(message))
}

/// Builds an invalid-config error for the provided message.
pub(crate) fn invalid_config_error(message: impl Into<String>) -> EvalError {
    EvalError::InvalidConfig(message.into())
}

/// Wraps an I/O failure with the offending path.
pub(crate) fn io_error(path: &Path, source: std::io::Error) -> EvalError {
    EvalError::Io {
        path: path.to_path_buf(),
        source,
    }
}

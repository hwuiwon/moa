//! JSONL I/O helpers for generated memory eval corpus sidecars.

use std::path::Path;

use moa_eval_core::{EvalError, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub(super) async fn read_jsonl<T>(path: &Path) -> Result<Vec<T>>
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

pub(super) async fn write_jsonl<T>(path: &Path, records: &[T]) -> Result<()>
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

async fn ensure_parent_dir(path: &Path) -> Result<()> {
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

pub(super) fn io_error(path: &Path, source: std::io::Error) -> EvalError {
    EvalError::Io {
        path: path.to_path_buf(),
        source,
    }
}

//! `file_write` tool implementation.

use std::path::Path;
use std::time::Duration;

use moa_core::{Result, ToolOutput};
use serde::Deserialize;
use tokio::fs;

use crate::tools::file_read::resolve_sandbox_path;

/// Executes the `file_write` tool against a sandbox directory.
pub async fn execute(sandbox_dir: &Path, input: &str) -> Result<ToolOutput> {
    let params: FileWriteInput = serde_json::from_str(input)?;
    let path = resolve_sandbox_path(sandbox_dir, &params.path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(&path, params.content).await?;

    Ok(ToolOutput::text(
        format!(
            "wrote {}",
            path.strip_prefix(sandbox_dir).unwrap_or(&path).display()
        ),
        Duration::default(),
    ))
}

#[derive(Debug, Deserialize)]
struct FileWriteInput {
    path: String,
    content: String,
}

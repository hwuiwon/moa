//! `file_read` tool implementation.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use moa_core::{MoaError, Result, ToolOutput};
use serde::Deserialize;
use tokio::fs;

use crate::tools::docker_file::{docker_file_read, resolve_container_workspace_path};

/// Executes the `file_read` tool against a sandbox directory.
pub async fn execute(sandbox_dir: &Path, input: &str) -> Result<ToolOutput> {
    let params: FileReadInput = serde_json::from_str(input)?;
    let path = resolve_sandbox_path(sandbox_dir, &params.path)?;
    let content = fs::read_to_string(path).await?;

    Ok(ToolOutput::text(content, Duration::default()))
}

/// Executes the `file_read` tool inside an existing Docker sandbox.
pub async fn execute_docker(
    container_id: &str,
    workspace_root: &str,
    input: &str,
    timeout: Duration,
) -> Result<ToolOutput> {
    let params: FileReadInput = serde_json::from_str(input)?;
    let path = resolve_container_workspace_path(workspace_root, &params.path)?;
    let content = docker_file_read(container_id, &path, timeout).await?;
    Ok(ToolOutput::text(content, Duration::default()))
}

/// Resolves a user-provided relative path inside a sandbox root.
pub fn resolve_sandbox_path(sandbox_dir: &Path, raw_path: &str) -> Result<PathBuf> {
    let logical_path = Path::new(raw_path);
    if logical_path.is_absolute() {
        return Err(MoaError::PermissionDenied(format!(
            "path must stay within the sandbox: {raw_path}"
        )));
    }

    for component in logical_path.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(MoaError::PermissionDenied(format!(
                "path traversal is not allowed: {raw_path}"
            )));
        }
    }

    Ok(sandbox_dir.join(logical_path))
}

#[derive(Debug, Deserialize)]
struct FileReadInput {
    path: String,
}

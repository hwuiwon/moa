//! `file_read` tool implementation.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use moa_core::{MoaError, Result, ToolOutput};
use serde::Deserialize;
use tokio::fs;
use tokio_util::sync::CancellationToken;

use crate::tools::docker_file::{
    display_container_relative_path, docker_file_read, resolve_container_workspace_path,
};

const MAX_UNSCOPED_FILE_READ_BYTES: usize = 32 * 1024;
const MAX_SCOPED_FILE_READ_LINES: usize = 400;

/// Executes the `file_read` tool against a sandbox directory.
pub async fn execute(sandbox_dir: &Path, input: &str) -> Result<ToolOutput> {
    let params: FileReadInput = serde_json::from_str(input)?;
    let path = resolve_sandbox_path(sandbox_dir, &params.path)?;
    let content = fs::read_to_string(&path).await?;
    let display_path = path.strip_prefix(sandbox_dir).unwrap_or(&path).display();

    render_file_read_output(&content, &display_path.to_string(), &params)
}

/// Executes the `file_read` tool inside an existing Docker sandbox.
pub async fn execute_docker(
    container_id: &str,
    workspace_root: &str,
    input: &str,
    timeout: Duration,
    hard_cancel_token: Option<&CancellationToken>,
) -> Result<ToolOutput> {
    let params: FileReadInput = serde_json::from_str(input)?;
    let path = resolve_container_workspace_path(workspace_root, &params.path)?;
    let content = docker_file_read(container_id, &path, timeout, hard_cancel_token).await?;
    let display_path = display_container_relative_path(workspace_root, &path);
    render_file_read_output(&content, &display_path, &params)
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

fn render_file_read_output(
    content: &str,
    display_path: &str,
    params: &FileReadInput,
) -> Result<ToolOutput> {
    let lines = split_lines(content);
    if params.start_line.is_none()
        && params.end_line.is_none()
        && content.len() > MAX_UNSCOPED_FILE_READ_BYTES
    {
        return Err(MoaError::ToolError(format!(
            "file_read failed: {display_path} is too large to read without a range ({} bytes). Retry with start_line and end_line.",
            content.len()
        )));
    }

    let Some((start_line, end_line)) = resolve_line_range(params, lines.len())? else {
        return Ok(ToolOutput::text(content.to_string(), Duration::default()));
    };
    let snippet = lines[start_line - 1..end_line].concat();

    Ok(ToolOutput::text(
        format!("lines {start_line}-{end_line} of {display_path}:\n{snippet}"),
        Duration::default(),
    ))
}

fn resolve_line_range(
    params: &FileReadInput,
    total_lines: usize,
) -> Result<Option<(usize, usize)>> {
    if params.start_line.is_none() && params.end_line.is_none() {
        return Ok(None);
    }
    if total_lines == 0 {
        return Err(MoaError::ToolError(
            "file_read failed: cannot read a line range from an empty file".to_string(),
        ));
    }

    let start_line = params.start_line.unwrap_or(1);
    if start_line == 0 {
        return Err(MoaError::ValidationError(
            "file_read start_line must be at least 1".to_string(),
        ));
    }
    if start_line > total_lines {
        return Err(MoaError::ToolError(format!(
            "file_read failed: start_line {start_line} is past the end of the file ({total_lines} lines)"
        )));
    }

    let end_line = params
        .end_line
        .unwrap_or_else(|| start_line.saturating_add(MAX_SCOPED_FILE_READ_LINES - 1))
        .min(total_lines);
    if end_line < start_line {
        return Err(MoaError::ValidationError(
            "file_read end_line must be greater than or equal to start_line".to_string(),
        ));
    }
    let line_count = end_line - start_line + 1;
    if line_count > MAX_SCOPED_FILE_READ_LINES {
        return Err(MoaError::ToolError(format!(
            "file_read failed: requested {line_count} lines from {start_line} to {end_line}. Limit reads to {MAX_SCOPED_FILE_READ_LINES} lines per call."
        )));
    }

    Ok(Some((start_line, end_line)))
}

fn split_lines(content: &str) -> Vec<&str> {
    content.split_inclusive('\n').collect()
}

#[derive(Debug, Deserialize)]
struct FileReadInput {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn file_read_returns_full_small_file_without_range() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join("notes.txt"), "alpha\nbeta\n")
            .await
            .expect("write file");

        let output = execute(dir.path(), r#"{"path":"notes.txt"}"#)
            .await
            .expect("file read");

        assert_eq!(output.to_text(), "alpha\nbeta");
    }

    #[tokio::test]
    async fn file_read_reads_requested_line_range() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join("notes.txt"), "one\ntwo\nthree\nfour\n")
            .await
            .expect("write file");

        let output = execute(
            dir.path(),
            r#"{"path":"notes.txt","start_line":2,"end_line":3}"#,
        )
        .await
        .expect("file read");

        assert_eq!(output.to_text(), "lines 2-3 of notes.txt:\ntwo\nthree");
    }

    #[tokio::test]
    async fn file_read_rejects_large_unscoped_reads() {
        let dir = tempdir().expect("tempdir");
        let large = "line\n".repeat(10_000);
        fs::write(dir.path().join("large.txt"), large)
            .await
            .expect("write file");

        let error = execute(dir.path(), r#"{"path":"large.txt"}"#)
            .await
            .expect_err("expected large read failure");

        assert!(error.to_string().contains("too large"));
        assert!(error.to_string().contains("start_line"));
    }

    #[tokio::test]
    async fn file_read_rejects_ranges_larger_than_limit() {
        let dir = tempdir().expect("tempdir");
        let content = "line\n".repeat(500);
        fs::write(dir.path().join("large.txt"), content)
            .await
            .expect("write file");

        let error = execute(
            dir.path(),
            r#"{"path":"large.txt","start_line":1,"end_line":450}"#,
        )
        .await
        .expect_err("expected large range failure");

        assert!(
            error
                .to_string()
                .contains("Limit reads to 400 lines per call")
        );
    }
}

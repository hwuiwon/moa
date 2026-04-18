//! `file_write` tool implementation.

use std::io::ErrorKind;
use std::path::Path;
use std::time::Duration;

use moa_core::{Result, ToolOutput};
use serde::Deserialize;
use tokio::fs;
use tokio_util::sync::CancellationToken;

use crate::tools::docker_file::{
    display_container_relative_path, docker_file_read, docker_file_write,
    resolve_container_workspace_path,
};
use crate::tools::edit_output::{ExistingFileContent, build_file_write_output};
use crate::tools::file_read::resolve_sandbox_path;

/// Executes the `file_write` tool against a sandbox directory.
pub async fn execute(sandbox_dir: &Path, input: &str) -> Result<ToolOutput> {
    let params: FileWriteInput = serde_json::from_str(input)?;
    let path = resolve_sandbox_path(sandbox_dir, &params.path)?;
    let existing = read_existing_file_content(&path).await?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(&path, &params.content).await?;

    Ok(build_file_write_output(
        &display_path(sandbox_dir, &path),
        &existing,
        &params.content,
        Duration::default(),
    ))
}

/// Executes the `file_write` tool inside an existing Docker sandbox.
pub async fn execute_docker(
    container_id: &str,
    workspace_root: &str,
    input: &str,
    timeout: Duration,
    hard_cancel_token: Option<&CancellationToken>,
) -> Result<ToolOutput> {
    let params: FileWriteInput = serde_json::from_str(input)?;
    let path = resolve_container_workspace_path(workspace_root, &params.path)?;
    let existing = match docker_file_read(container_id, &path, timeout, hard_cancel_token).await {
        Ok(content) => ExistingFileContent::Text(content),
        Err(moa_core::MoaError::ToolError(message))
            if message.contains("No such file or directory") =>
        {
            ExistingFileContent::Missing
        }
        Err(error) => return Err(error),
    };
    docker_file_write(
        container_id,
        &path,
        &params.content,
        timeout,
        hard_cancel_token,
    )
    .await?;

    Ok(build_file_write_output(
        &display_container_relative_path(workspace_root, &path),
        &existing,
        &params.content,
        Duration::default(),
    ))
}

async fn read_existing_file_content(path: &Path) -> Result<ExistingFileContent> {
    match fs::read(path).await {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(content) => Ok(ExistingFileContent::Text(content)),
            Err(_) => Ok(ExistingFileContent::Binary),
        },
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(ExistingFileContent::Missing),
        Err(error) => Err(error.into()),
    }
}

fn display_path(sandbox_dir: &Path, path: &Path) -> String {
    path.strip_prefix(sandbox_dir)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[derive(Debug, Deserialize)]
struct FileWriteInput {
    path: String,
    content: String,
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn overwrite_returns_unified_diff() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join("demo.txt"), "alpha\nbeta\ngamma\n")
            .await
            .expect("write");

        let output = execute(
            dir.path(),
            r#"{"path":"demo.txt","content":"alpha\nomega\ngamma\n"}"#,
        )
        .await
        .expect("file_write");

        let rendered = output.to_text();
        assert!(rendered.starts_with("--- a/demo.txt\n+++ b/demo.txt\n"));
        assert!(rendered.contains("-beta"));
        assert!(rendered.contains("+omega"));
        assert!(!rendered.contains("wrote demo.txt"));
    }

    #[tokio::test]
    async fn new_file_returns_creation_notice() {
        let dir = tempdir().expect("tempdir");

        let output = execute(
            dir.path(),
            r#"{"path":"nested/demo.txt","content":"hello\nworld\n"}"#,
        )
        .await
        .expect("file_write");

        assert_eq!(
            output.to_text(),
            "[new file created: nested/demo.txt, 2 lines]"
        );
    }

    #[tokio::test]
    async fn binary_overwrite_returns_binary_notice() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join("logo.bin"), [0xff_u8, 0x00, 0xfe])
            .await
            .expect("write");

        let output = execute(dir.path(), r#"{"path":"logo.bin","content":"text"}"#)
            .await
            .expect("file_write");

        assert_eq!(output.to_text(), "[binary file written: logo.bin, 4 bytes]");
    }
}

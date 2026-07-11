//! `file_write` tool implementation.

use std::path::Path;
use std::time::Duration;

use moa_core::{error::Result, types::tools::ToolOutput};
use serde::Deserialize;
use tokio::fs;
use tokio_util::sync::CancellationToken;

use crate::tools::docker_file::{
    display_container_relative_path, docker_file_read, docker_file_write,
    resolve_container_workspace_path,
};
use crate::tools::edit_output::{ExistingFileContent, build_file_write_output};
use crate::tools::file_read::resolve_writable_sandbox_path;
use crate::tools::fs_util::read_optional_file_bytes;

/// Executes the `file_write` tool against a sandbox directory.
pub async fn execute(sandbox_dir: &Path, input: &str) -> Result<ToolOutput> {
    let params: FileWriteInput = serde_json::from_str(input)?;
    let resolved = resolve_writable_sandbox_path(sandbox_dir, &params.path).await?;
    let existing = read_existing_file_content(&resolved.path).await?;
    fs::write(&resolved.path, &params.content).await?;

    Ok(build_file_write_output(
        &resolved.display_path,
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
        Err(moa_core::error::MoaError::ToolError(message))
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

/// Executes the `file_write` tool against a Docker sandbox bind mount.
pub(crate) async fn execute_docker_bind_mount(
    host_workspace_root: &Path,
    workspace_root: &str,
    input: &str,
) -> Result<ToolOutput> {
    let params: FileWriteInput = serde_json::from_str(input)?;
    let container_path = resolve_container_workspace_path(workspace_root, &params.path)?;
    let display_path = display_container_relative_path(workspace_root, &container_path);
    let resolved = resolve_writable_sandbox_path(host_workspace_root, &display_path).await?;
    let existing = read_existing_file_content(&resolved.path).await?;
    fs::write(&resolved.path, &params.content).await?;

    Ok(build_file_write_output(
        &display_path,
        &existing,
        &params.content,
        Duration::default(),
    ))
}

async fn read_existing_file_content(path: &Path) -> Result<ExistingFileContent> {
    match read_optional_file_bytes(path).await? {
        Some(bytes) => match String::from_utf8(bytes) {
            Ok(content) => Ok(ExistingFileContent::Text(content)),
            Err(_) => Ok(ExistingFileContent::Binary),
        },
        None => Ok(ExistingFileContent::Missing),
    }
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

    #[cfg(unix)]
    #[tokio::test]
    async fn file_write_rejects_symlink_escape() {
        // Pins: host-local writes must not overwrite files outside the sandbox through symlinks.
        let dir = tempdir().expect("tempdir");
        let outside = tempdir().expect("outside tempdir");
        let outside_file = outside.path().join("secret.txt");
        fs::write(&outside_file, "keep me")
            .await
            .expect("write outside file");
        std::os::unix::fs::symlink(&outside_file, dir.path().join("link.txt"))
            .expect("create symlink");

        let error = execute(dir.path(), r#"{"path":"link.txt","content":"escape"}"#)
            .await
            .expect_err("symlink escape should be rejected");

        assert!(matches!(
            error,
            moa_core::error::MoaError::PermissionDenied(_)
        ));
        assert_eq!(
            fs::read_to_string(&outside_file)
                .await
                .expect("read outside file"),
            "keep me"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn docker_bind_mount_write_rejects_symlink_escape() {
        // Pins: Docker bind-mounted writes must not follow host symlinks outside the workspace.
        let dir = tempdir().expect("tempdir");
        let outside = tempdir().expect("outside tempdir");
        let outside_file = outside.path().join("secret.txt");
        fs::write(&outside_file, "keep me")
            .await
            .expect("write outside file");
        std::os::unix::fs::symlink(&outside_file, dir.path().join("link.txt"))
            .expect("create symlink");

        let error = execute_docker_bind_mount(
            dir.path(),
            "/workspace",
            r#"{"path":"link.txt","content":"escape"}"#,
        )
        .await
        .expect_err("bind-mounted symlink escape should be rejected");

        assert!(matches!(
            error,
            moa_core::error::MoaError::PermissionDenied(_)
        ));
        assert_eq!(
            fs::read_to_string(&outside_file)
                .await
                .expect("read outside file"),
            "keep me"
        );
    }
}

//! Shared Docker-backed file helpers for file tools.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use globset::Glob;
use moa_core::{MoaError, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Resolves a user-provided path inside a container workspace mount.
pub(crate) fn resolve_container_workspace_path(
    workspace_root: &str,
    raw_path: &str,
) -> Result<String> {
    let workspace_root = Path::new(workspace_root);
    if !workspace_root.is_absolute() {
        return Err(MoaError::ValidationError(format!(
            "container workspace root must be absolute: {workspace_root:?}"
        )));
    }

    let candidate = Path::new(raw_path);
    let relative_path = if candidate.is_absolute() {
        candidate.strip_prefix(workspace_root).map_err(|_| {
            MoaError::PermissionDenied(format!(
                "path must stay within the container workspace: {raw_path}"
            ))
        })?
    } else {
        candidate
    };

    for component in relative_path.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(MoaError::PermissionDenied(format!(
                "path traversal is not allowed in the container workspace: {raw_path}"
            )));
        }
    }

    Ok(workspace_root
        .join(relative_path)
        .to_string_lossy()
        .into_owned())
}

/// Converts an absolute container path back into a workspace-relative path for display.
pub(crate) fn display_container_relative_path(workspace_root: &str, absolute_path: &str) -> String {
    let workspace_root = Path::new(workspace_root);
    let absolute_path = Path::new(absolute_path);
    absolute_path
        .strip_prefix(workspace_root)
        .unwrap_or(absolute_path)
        .display()
        .to_string()
}

/// Builds the `docker exec ... cat` argument vector for tests and execution.
pub(crate) fn docker_read_args(container_id: &str, path: &str) -> Vec<String> {
    vec![
        "exec".to_string(),
        container_id.to_string(),
        "cat".to_string(),
        path.to_string(),
    ]
}

/// Builds the `docker exec ... sh -lc ...` argument vector used for writes.
pub(crate) fn docker_write_args(container_id: &str, path: &str) -> Vec<String> {
    vec![
        "exec".to_string(),
        "-i".to_string(),
        container_id.to_string(),
        "sh".to_string(),
        "-lc".to_string(),
        "mkdir -p \"$(dirname -- \"$1\")\" && cat > \"$1\"".to_string(),
        "sh".to_string(),
        path.to_string(),
    ]
}

/// Builds the `docker exec ... find` argument vector for file listing.
pub(crate) fn docker_find_args(container_id: &str, root: &str) -> Vec<String> {
    vec![
        "exec".to_string(),
        container_id.to_string(),
        "find".to_string(),
        root.to_string(),
        "-type".to_string(),
        "f".to_string(),
        "-print".to_string(),
    ]
}

/// Reads a file inside a running Docker container.
pub(crate) async fn docker_file_read(
    container_id: &str,
    path: &str,
    timeout: Duration,
    hard_cancel_token: Option<&CancellationToken>,
) -> Result<String> {
    let output = run_docker_command(
        container_id,
        docker_read_args(container_id, path),
        None,
        timeout,
        hard_cancel_token,
    )
    .await?;
    if !output.status.success() {
        return Err(MoaError::ToolError(format!(
            "docker file_read failed: {}",
            stderr_or_status(&output)
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Writes content to a file inside a running Docker container.
pub(crate) async fn docker_file_write(
    container_id: &str,
    path: &str,
    content: &str,
    timeout: Duration,
    hard_cancel_token: Option<&CancellationToken>,
) -> Result<()> {
    let output = run_docker_command(
        container_id,
        docker_write_args(container_id, path),
        Some(content.as_bytes()),
        timeout,
        hard_cancel_token,
    )
    .await?;
    if !output.status.success() {
        return Err(MoaError::ToolError(format!(
            "docker file_write failed: {}",
            stderr_or_status(&output)
        )));
    }

    Ok(())
}

/// Searches for files inside a running Docker container using `find`.
pub(crate) async fn docker_file_search(
    container_id: &str,
    pattern: &str,
    root: &str,
    timeout: Duration,
    hard_cancel_token: Option<&CancellationToken>,
) -> Result<Vec<String>> {
    let matcher = Glob::new(pattern)
        .map_err(|error| MoaError::ValidationError(error.to_string()))?
        .compile_matcher();
    let output = run_docker_command(
        container_id,
        docker_find_args(container_id, root),
        None,
        timeout,
        hard_cancel_token,
    )
    .await?;
    if !output.status.success() {
        return Err(MoaError::ToolError(format!(
            "docker file_search failed: {}",
            stderr_or_status(&output)
        )));
    }

    let root_path = Path::new(root);
    let mut matches = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let path = PathBuf::from(line);
            let relative = path.strip_prefix(root_path).ok()?;
            if matcher.is_match(relative) {
                Some(relative.display().to_string())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    matches.sort();
    Ok(matches)
}

async fn run_docker_command(
    container_id: &str,
    args: Vec<String>,
    stdin: Option<&[u8]>,
    timeout: Duration,
    hard_cancel_token: Option<&CancellationToken>,
) -> Result<std::process::Output> {
    let mut command = Command::new("docker");
    command.args(&args).kill_on_drop(true);
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    }

    let mut child = command.spawn().map_err(|error| {
        MoaError::ProviderError(format!("failed to spawn docker exec command: {error}"))
    })?;

    if let Some(stdin_bytes) = stdin
        && let Some(mut child_stdin) = child.stdin.take()
    {
        child_stdin.write_all(stdin_bytes).await.map_err(|error| {
            MoaError::ProviderError(format!("failed to stream stdin to docker exec: {error}"))
        })?;
        child_stdin.shutdown().await.map_err(|error| {
            MoaError::ProviderError(format!("failed to close docker exec stdin: {error}"))
        })?;
    }

    let wait_output = child.wait_with_output();
    tokio::pin!(wait_output);

    if let Some(hard_cancel_token) = hard_cancel_token {
        tokio::select! {
            result = tokio::time::timeout(timeout, &mut wait_output) => {
                result
                    .map_err(|_| {
                        MoaError::ProviderError(format!(
                            "docker exec command timed out after {}s",
                            timeout.as_secs()
                        ))
                    })?
                    .map_err(|error| {
                        MoaError::ProviderError(format!("failed to wait for docker exec command: {error}"))
                    })
            }
            _ = hard_cancel_token.cancelled() => {
                let _ = stop_container(container_id).await;
                Err(MoaError::Cancelled)
            }
        }
    } else {
        tokio::time::timeout(timeout, wait_output)
            .await
            .map_err(|_| {
                MoaError::ProviderError(format!(
                    "docker exec command timed out after {}s",
                    timeout.as_secs()
                ))
            })?
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to wait for docker exec command: {error}"))
            })
    }
}

fn stderr_or_status(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        format!("exit code {}", output.status.code().unwrap_or(-1))
    } else {
        stderr
    }
}

async fn stop_container(container_id: &str) -> Result<()> {
    let output = Command::new("docker")
        .args(["stop", "-t", "2", container_id])
        .output()
        .await?;
    if output.status.success()
        || String::from_utf8_lossy(&output.stderr).contains("No such container")
    {
        return Ok(());
    }
    Err(MoaError::ProviderError(format!(
        "failed to stop docker sandbox during cancellation: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

#[cfg(test)]
mod tests {
    use super::{
        display_container_relative_path, docker_find_args, docker_read_args, docker_write_args,
        resolve_container_workspace_path,
    };

    #[test]
    fn container_path_validation_rejects_traversal() {
        let error = resolve_container_workspace_path("/workspace", "../../../etc/passwd")
            .expect_err("traversal should be rejected");
        assert!(matches!(error, moa_core::MoaError::PermissionDenied(_)));
    }

    #[test]
    fn container_path_validation_accepts_workspace_absolute_paths() {
        let path =
            resolve_container_workspace_path("/workspace", "/workspace/src/main.rs").unwrap();
        assert_eq!(path, "/workspace/src/main.rs");
    }

    #[test]
    fn container_path_validation_rejects_absolute_paths_outside_workspace() {
        let error = resolve_container_workspace_path("/workspace", "/etc/hosts")
            .expect_err("outside absolute path should be rejected");
        assert!(matches!(error, moa_core::MoaError::PermissionDenied(_)));
    }

    #[test]
    fn docker_exec_argument_builders_match_expected_commands() {
        assert_eq!(
            docker_read_args("cid", "/workspace/notes.txt"),
            vec!["exec", "cid", "cat", "/workspace/notes.txt"]
        );
        assert_eq!(
            docker_find_args("cid", "/workspace"),
            vec!["exec", "cid", "find", "/workspace", "-type", "f", "-print"]
        );
        assert_eq!(
            docker_write_args("cid", "/workspace/notes.txt"),
            vec![
                "exec",
                "-i",
                "cid",
                "sh",
                "-lc",
                "mkdir -p \"$(dirname -- \"$1\")\" && cat > \"$1\"",
                "sh",
                "/workspace/notes.txt",
            ]
        );
    }

    #[test]
    fn display_path_is_relative_to_workspace_root() {
        assert_eq!(
            display_container_relative_path("/workspace", "/workspace/src/main.rs"),
            "src/main.rs"
        );
    }
}

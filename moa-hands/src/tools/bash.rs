//! `bash` tool execution helpers.

use std::path::Path;
use std::time::{Duration, Instant};

use moa_core::{MoaError, Result, ToolOutput};
use serde::Deserialize;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Executes the `bash` tool in a local sandbox directory.
pub async fn execute_local(
    sandbox_dir: &Path,
    input: &str,
    default_timeout: Duration,
    hard_cancel_token: Option<&CancellationToken>,
) -> Result<ToolOutput> {
    let params: BashToolInput = serde_json::from_str(input)?;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let timeout = params.timeout(default_timeout);
    let started_at = Instant::now();

    let mut command = Command::new(shell);
    command
        .arg("-lc")
        .arg(&params.cmd)
        .current_dir(sandbox_dir)
        .kill_on_drop(true);

    let output = if let Some(hard_cancel_token) = hard_cancel_token {
        let output = command.output();
        tokio::pin!(output);
        tokio::select! {
            result = tokio::time::timeout(timeout, &mut output) => {
                result.map_err(|_| {
                    MoaError::ToolError(format!(
                        "bash command timed out after {}s",
                        timeout.as_secs()
                    ))
                })??
            }
            _ = hard_cancel_token.cancelled() => {
                return Err(MoaError::Cancelled);
            }
        }
    } else {
        tokio::time::timeout(timeout, command.output())
            .await
            .map_err(|_| {
                MoaError::ToolError(format!(
                    "bash command timed out after {}s",
                    timeout.as_secs()
                ))
            })??
    };

    Ok(ToolOutput::from_process(
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.code().unwrap_or(-1),
        started_at.elapsed(),
    ))
}

/// Executes the `bash` tool inside an existing Docker sandbox.
pub async fn execute_docker(
    container_id: &str,
    workspace_root: &str,
    input: &str,
    default_timeout: Duration,
    hard_cancel_token: Option<&CancellationToken>,
) -> Result<ToolOutput> {
    let params: BashToolInput = serde_json::from_str(input)?;
    let timeout = params.timeout(default_timeout);
    let started_at = Instant::now();

    let mut command = Command::new("docker");
    command
        .args(["exec", "-w", workspace_root, container_id, "sh", "-lc"])
        .arg(&params.cmd)
        .kill_on_drop(true);

    let output = if let Some(hard_cancel_token) = hard_cancel_token {
        let output = command.output();
        tokio::pin!(output);
        tokio::select! {
            result = tokio::time::timeout(timeout, &mut output) => {
                result.map_err(|_| {
                    MoaError::ToolError(format!(
                        "docker bash command timed out after {}s",
                        timeout.as_secs()
                    ))
                })??
            }
            _ = hard_cancel_token.cancelled() => {
                let _ = stop_container(container_id).await;
                return Err(MoaError::Cancelled);
            }
        }
    } else {
        tokio::time::timeout(timeout, command.output())
            .await
            .map_err(|_| {
                MoaError::ToolError(format!(
                    "docker bash command timed out after {}s",
                    timeout.as_secs()
                ))
            })??
    };

    Ok(ToolOutput::from_process(
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.code().unwrap_or(-1),
        started_at.elapsed(),
    ))
}

#[derive(Debug, Deserialize)]
struct BashToolInput {
    cmd: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

impl BashToolInput {
    fn timeout(&self, default_timeout: Duration) -> Duration {
        self.timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(default_timeout)
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

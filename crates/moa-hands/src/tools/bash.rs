//! `bash` tool execution helpers.

use std::path::Path;
use std::time::{Duration, Instant};

use moa_core::{MoaError, Result, ToolOutput};
use serde::Deserialize;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::tools::docker_file::stop_container;

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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use moa_core::ToolOutput;

    #[test]
    fn bash_output_preserves_full_process_streams() {
        let stdout = (1..=1_000)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");

        let output = ToolOutput::from_process(stdout, String::new(), 0, Duration::from_secs(1));
        let text = output.to_text();

        assert!(!output.truncated);
        assert!(text.contains("line 1"));
        assert!(text.contains("line 1000"));
    }

    #[test]
    fn bash_output_small_streams_are_not_truncated() {
        let output = ToolOutput::from_process(
            "out".to_string(),
            "err".to_string(),
            0,
            Duration::from_secs(1),
        );

        assert!(!output.truncated);
        assert_eq!(output.process_stdout(), Some("out"));
        assert_eq!(output.process_stderr(), Some("err"));
    }
}

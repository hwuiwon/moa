//! `bash` tool execution helpers.

use std::path::Path;
use std::time::{Duration, Instant};

use moa_core::{MoaError, Result, ToolOutput};
use serde::Deserialize;
use tokio::process::Command;

/// Executes the `bash` tool in a local sandbox directory.
pub async fn execute_local(
    sandbox_dir: &Path,
    input: &str,
    default_timeout: Duration,
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

    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| {
            MoaError::ToolError(format!(
                "bash command timed out after {}s",
                timeout.as_secs()
            ))
        })??;

    Ok(ToolOutput {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        exit_code: output.status.code().unwrap_or(-1),
        duration: started_at.elapsed(),
    })
}

/// Executes the `bash` tool inside an existing Docker sandbox.
pub async fn execute_docker(
    container_id: &str,
    input: &str,
    default_timeout: Duration,
) -> Result<ToolOutput> {
    let params: BashToolInput = serde_json::from_str(input)?;
    let timeout = params.timeout(default_timeout);
    let started_at = Instant::now();

    let mut command = Command::new("docker");
    command
        .args(["exec", "-w", "/workspace", container_id, "sh", "-lc"])
        .arg(&params.cmd)
        .kill_on_drop(true);

    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| {
            MoaError::ToolError(format!(
                "docker bash command timed out after {}s",
                timeout.as_secs()
            ))
        })??;

    Ok(ToolOutput {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        exit_code: output.status.code().unwrap_or(-1),
        duration: started_at.elapsed(),
    })
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

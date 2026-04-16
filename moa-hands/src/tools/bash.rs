//! `bash` tool execution helpers.

use std::path::Path;
use std::time::{Duration, Instant};

use moa_core::{
    MoaError, Result, ToolContent, ToolOutput, ToolOutputConfig, truncate_head_tail,
    truncate_head_tail_lines,
};
use serde::Deserialize;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Executes the `bash` tool in a local sandbox directory.
pub async fn execute_local(
    sandbox_dir: &Path,
    input: &str,
    default_timeout: Duration,
    tool_output: &ToolOutputConfig,
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

    Ok(build_bash_output(
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.code().unwrap_or(-1),
        started_at.elapsed(),
        tool_output,
    ))
}

/// Executes the `bash` tool inside an existing Docker sandbox.
pub async fn execute_docker(
    container_id: &str,
    workspace_root: &str,
    input: &str,
    default_timeout: Duration,
    tool_output: &ToolOutputConfig,
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

    Ok(build_bash_output(
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.code().unwrap_or(-1),
        started_at.elapsed(),
        tool_output,
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

fn build_bash_output(
    stdout: String,
    stderr: String,
    exit_code: i32,
    duration: Duration,
    tool_output: &ToolOutputConfig,
) -> ToolOutput {
    let (stdout, stdout_truncated) = truncate_shell_stream(&stdout, tool_output);
    let (stderr, stderr_truncated) = truncate_shell_stream(&stderr, tool_output);

    let mut output = ToolOutput::from_process(stdout, stderr, exit_code, duration);
    let mut truncated = stdout_truncated || stderr_truncated;

    let (combined, combined_truncated) = truncate_head_tail(
        &output.to_text(),
        tool_output.max_replay_chars,
        tool_output.head_ratio,
    );
    if combined_truncated {
        output.content = vec![ToolContent::Text { text: combined }];
        truncated = true;
    }

    output.with_truncated(truncated)
}

fn truncate_shell_stream(text: &str, tool_output: &ToolOutputConfig) -> (String, bool) {
    let (line_truncated, truncated_by_lines) =
        truncate_head_tail_lines(text, tool_output.max_bash_lines, tool_output.head_ratio);
    let (char_truncated, truncated_by_chars) = truncate_head_tail(
        &line_truncated,
        tool_output.max_replay_chars,
        tool_output.head_ratio,
    );

    (char_truncated, truncated_by_lines || truncated_by_chars)
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
    use std::time::Duration;

    use super::build_bash_output;
    use moa_core::ToolOutputConfig;

    #[test]
    fn bash_output_truncates_with_head_and_tail_preserved() {
        let stdout = (1..=1_000)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");

        let output = build_bash_output(
            stdout,
            String::new(),
            0,
            Duration::from_secs(1),
            &ToolOutputConfig::default(),
        );
        let text = output.to_text();

        assert!(output.truncated);
        assert!(text.contains("line 1"));
        assert!(text.contains("line 1000"));
        assert!(text.contains("[..."));
    }

    #[test]
    fn bash_output_small_streams_are_not_truncated() {
        let output = build_bash_output(
            "out".to_string(),
            "err".to_string(),
            0,
            Duration::from_secs(1),
            &ToolOutputConfig::default(),
        );

        assert!(!output.truncated);
        assert_eq!(output.process_stdout(), Some("out"));
        assert_eq!(output.process_stderr(), Some("err"));
    }
}

//! `bash` tool execution helpers.

use std::path::Path;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use moa_core::{MoaError, Result, ToolOutput};
use serde::Deserialize;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::tools::docker_file::stop_container;

/// Login shell used for local `bash` tool execution.
///
/// Resolved from `$SHELL` once at first use instead of on every command so a
/// hot tool loop does not re-read the environment per call.
static LOGIN_SHELL: LazyLock<String> =
    LazyLock::new(|| std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()));

/// Upper bound on captured stdout/stderr bytes retained per stream.
///
/// A runaway command can emit gigabytes; the router's output budget later
/// truncates for display and the claim-check artifact stores the retained text,
/// so bounding capture here caps process memory without losing the visible or
/// artifact-persisted portion for normal outputs.
const MAX_CAPTURED_STREAM_BYTES: usize = 4 * 1024 * 1024;

struct CapturedStream {
    text: String,
    truncated: bool,
}

/// Converts captured process output to text, bounding retained bytes per stream.
fn capture_stream(bytes: &[u8]) -> CapturedStream {
    if bytes.len() <= MAX_CAPTURED_STREAM_BYTES {
        return CapturedStream {
            text: String::from_utf8_lossy(bytes).into_owned(),
            truncated: false,
        };
    }
    let mut text = String::from_utf8_lossy(&bytes[..MAX_CAPTURED_STREAM_BYTES]).into_owned();
    text.push_str("\n[stream truncated at source after 4 MiB]");
    CapturedStream {
        text,
        truncated: true,
    }
}

fn original_output_tokens(stdout_bytes: usize, stderr_bytes: usize) -> Option<u32> {
    let total_bytes = stdout_bytes.saturating_add(stderr_bytes);
    let estimated_tokens = (total_bytes as u64).div_ceil(4).min(u64::from(u32::MAX));
    Some(estimated_tokens as u32)
}

fn process_output(
    stdout_bytes: &[u8],
    stderr_bytes: &[u8],
    exit_code: i32,
    duration: Duration,
) -> ToolOutput {
    let stdout = capture_stream(stdout_bytes);
    let stderr = capture_stream(stderr_bytes);
    let original_output_tokens = (stdout.truncated || stderr.truncated)
        .then(|| original_output_tokens(stdout_bytes.len(), stderr_bytes.len()))
        .flatten();

    ToolOutput::from_process_with_source_truncation(
        stdout.text,
        stderr.text,
        exit_code,
        duration,
        stdout.truncated,
        stderr.truncated,
        original_output_tokens,
    )
}

/// Executes the `bash` tool in a local sandbox directory.
pub async fn execute_local(
    sandbox_dir: &Path,
    input: &str,
    default_timeout: Duration,
    hard_cancel_token: Option<&CancellationToken>,
) -> Result<ToolOutput> {
    let params: BashToolInput = serde_json::from_str(input)?;
    let timeout = params.timeout(default_timeout);
    let started_at = Instant::now();

    let mut command = Command::new(&*LOGIN_SHELL);
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

    Ok(process_output(
        &output.stdout,
        &output.stderr,
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

    Ok(process_output(
        &output.stdout,
        &output.stderr,
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

    use super::{MAX_CAPTURED_STREAM_BYTES, process_output};

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
        let output = process_output(b"out", b"err", 0, Duration::from_secs(1));

        assert!(!output.truncated);
        assert_eq!(output.process_stdout(), Some("out"));
        assert_eq!(output.process_stderr(), Some("err"));
    }

    #[test]
    fn bash_output_source_truncation_is_marked_structurally() {
        let stdout = vec![b'x'; MAX_CAPTURED_STREAM_BYTES + 1];
        let output = process_output(&stdout, b"", 0, Duration::from_secs(1));

        assert!(output.truncated);
        assert!(output.original_output_tokens.is_some());
        assert!(output.to_text().contains("[stream truncated at source"));
        let structured = output
            .structured
            .as_ref()
            .expect("process output should be structured");
        assert_eq!(
            structured
                .get("stdout_truncated")
                .and_then(|value| value.as_bool()),
            Some(true)
        );
        assert_eq!(
            structured
                .get("stderr_truncated")
                .and_then(|value| value.as_bool()),
            Some(false)
        );
    }
}

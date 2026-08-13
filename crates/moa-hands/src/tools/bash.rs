//! `bash` tool execution helpers.

use std::num::NonZeroU64;
use std::path::Path;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use moa_core::{error::MoaError, error::Result, types::tools::ToolOutput};
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
///
/// `remaining_lifetime` is the time left on the sandbox this call runs inside,
/// or `None` when the sandbox was provisioned with a deliberately unbounded
/// maximum lifetime. `run_deadline` is the separate, usually much shorter, time
/// left on the *run* that asked for the command.
pub async fn execute_local(
    sandbox_dir: &Path,
    input: &str,
    default_timeout: Duration,
    remaining_lifetime: Option<Duration>,
    run_deadline: Option<Duration>,
    hard_cancel_token: Option<&CancellationToken>,
) -> Result<ToolOutput> {
    let params = BashToolInput::parse(input)?;
    let timeout = params.timeout(default_timeout, remaining_lifetime, run_deadline);
    reject_exhausted_budget(timeout, remaining_lifetime, run_deadline)?;
    let run_deadline_is_binding = run_deadline.is_some_and(|remaining| remaining <= timeout);
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
                match result {
                    Ok(output) => output?,
                    Err(_) if run_deadline_is_binding => {
                        hard_cancel_token.cancel();
                        return Err(MoaError::BudgetExhausted(
                            "run deadline passed while a bash command was running".to_string(),
                        ));
                    }
                    Err(_) => {
                        return Err(MoaError::ToolError(format!(
                            "bash command timed out after {}s",
                            timeout.as_secs()
                        )));
                    }
                }
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
///
/// `remaining_lifetime` is the time left on the container this call runs
/// inside, or `None` when it was provisioned with a deliberately unbounded
/// maximum lifetime. `run_deadline` is the separate, usually much shorter, time
/// left on the *run* that asked for the command.
#[allow(clippy::too_many_arguments)]
pub async fn execute_docker(
    container_id: &str,
    workspace_root: &str,
    input: &str,
    default_timeout: Duration,
    remaining_lifetime: Option<Duration>,
    run_deadline: Option<Duration>,
    hard_cancel_token: Option<&CancellationToken>,
) -> Result<ToolOutput> {
    let params = BashToolInput::parse(input)?;
    let timeout = params.timeout(default_timeout, remaining_lifetime, run_deadline);
    reject_exhausted_budget(timeout, remaining_lifetime, run_deadline)?;
    let run_deadline_is_binding = run_deadline.is_some_and(|remaining| remaining <= timeout);
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
                match result {
                    Ok(output) => output?,
                    Err(_) if run_deadline_is_binding => {
                        hard_cancel_token.cancel();
                        let _ = stop_container(container_id).await;
                        return Err(MoaError::BudgetExhausted(
                            "run deadline passed while a docker bash command was running".to_string(),
                        ));
                    }
                    Err(_) => {
                        return Err(MoaError::ToolError(format!(
                            "docker bash command timed out after {}s",
                            timeout.as_secs()
                        )));
                    }
                }
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

/// Largest caller-supplied `timeout_secs` any bash execution path accepts.
///
/// This is the enforced policy bound, not a hint. The published input schema
/// advertises the same `maximum`, but a JSON-Schema annotation is only checked
/// on the router's policy path — `serde` ignores it entirely, and a hand
/// provider invoked directly never sees the schema. A model emitting
/// `timeout_secs: 86400` would otherwise hold a sandbox for a day.
pub const MAX_BASH_TIMEOUT_SECS: u64 = 300;

/// Returns the wall-clock bound a tool invocation promises, when it declares one.
///
/// The attempt watchdog widens its staleness window to this value, because the durable
/// heartbeat is written at step boundaries and never during a step: without the bound, a
/// command that legitimately runs longer than the configured floor reads as a stall.
///
/// Only bash declares a bound today. Every other tool returns `None` and is held to the
/// configured floor, which is the intended contract rather than an omission: a tool with no
/// declared ceiling has nothing to justify a wider window with.
#[must_use]
pub fn declared_tool_step_bound(tool_name: &str, input: &serde_json::Value) -> Option<Duration> {
    if tool_name != "bash" {
        return None;
    }
    // An unparseable input never reaches a sandbox, so it cannot be running long; the
    // floor applies rather than a fabricated bound.
    let params = BashToolInput::parse(&input.to_string()).ok()?;
    Some(params.timeout(DEFAULT_BASH_TIMEOUT, None, None))
}

/// Wall-clock bound applied to a bash call that names no `timeout_secs` of its own.
///
/// Deliberately well below [`MAX_BASH_TIMEOUT_SECS`]: the ceiling exists so a caller that
/// knows it needs a long command can ask for one, not so every unqualified call holds a
/// sandbox for the maximum. A model that wants five minutes has to say so, which is also
/// what lets the watchdog keep a tight window for everything that does not.
pub const DEFAULT_BASH_TIMEOUT: Duration = Duration::from_secs(120);

/// Resolves the wall-clock ceiling for one synchronous sandbox tool call.
///
/// Bash may request its own validated ceiling; every other synchronous tool
/// uses the provider default. In both cases the caller's remaining deadline is
/// authoritative and an allowance below one second is rejected before remote
/// I/O starts.
pub fn effective_synchronous_timeout(
    tool_name: &str,
    input: &str,
    default_timeout: Duration,
    run_deadline: Option<Duration>,
) -> Result<Duration> {
    let timeout = if tool_name == "bash" {
        BashToolInput::parse(input)?.timeout(default_timeout, None, run_deadline)
    } else {
        run_deadline.map_or(default_timeout, |remaining| default_timeout.min(remaining))
    };
    if timeout < Duration::from_secs(1) {
        return Err(MoaError::ToolError(
            "synchronous tool execution has less than one second remaining before dispatch"
                .to_string(),
        ));
    }
    Ok(timeout)
}

/// A caller-supplied bash timeout that has already cleared tool policy.
///
/// Validation lives in the type's own deserialization rather than in each
/// executor, so an out-of-policy value cannot reach a `Command` through a path
/// that forgot to check. There is no clamping: silently rewriting 86400 into
/// 300 would tell the model its instruction was honoured when it was not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "u64")]
pub struct BashTimeoutSecs(NonZeroU64);

impl BashTimeoutSecs {
    /// Returns the validated timeout as a duration.
    #[must_use]
    pub fn duration(self) -> Duration {
        Duration::from_secs(self.0.get())
    }
}

impl TryFrom<u64> for BashTimeoutSecs {
    type Error = String;

    fn try_from(value: u64) -> std::result::Result<Self, Self::Error> {
        let seconds = NonZeroU64::new(value)
            .ok_or_else(|| "timeout_secs must be at least 1 second".to_string())?;
        if seconds.get() > MAX_BASH_TIMEOUT_SECS {
            return Err(format!(
                "timeout_secs {seconds} exceeds the maximum of {MAX_BASH_TIMEOUT_SECS} seconds"
            ));
        }
        Ok(Self(seconds))
    }
}

/// Parsed `bash` arguments whose timeout is already within policy.
#[derive(Debug, Deserialize)]
pub struct BashToolInput {
    /// Shell command to execute.
    pub cmd: String,
    /// Optional caller-supplied timeout override, already validated.
    #[serde(default)]
    pub timeout_secs: Option<BashTimeoutSecs>,
}

impl BashToolInput {
    /// Parses and validates one `bash` invocation payload.
    ///
    /// An out-of-policy `timeout_secs` is rejected here, before any process is
    /// spawned and before any remote sandbox is asked to run a command.
    pub fn parse(input: &str) -> Result<Self> {
        serde_json::from_str(input)
            .map_err(|error| MoaError::ValidationError(format!("invalid bash tool input: {error}")))
    }

    /// Returns the duration this call may run for.
    ///
    /// Four bounds apply and the smallest wins: the caller's own request, the
    /// deployment's default tool timeout, `remaining_lifetime` — the time left
    /// on the sandbox this call runs inside — and `run_deadline`, the time left
    /// on the run or trial that asked for the command.
    ///
    /// The last two are genuinely different clocks and neither implies the
    /// other. `remaining_lifetime` stops an in-policy 300-second command from
    /// outliving the sandbox a reaper is about to destroy underneath it.
    /// `run_deadline` stops the opposite failure: a perfectly healthy
    /// two-hour sandbox happily running a five-minute command for a turn whose
    /// deadline passed thirty seconds ago. Nothing about the sandbox knows the
    /// run expired, so without this bound the command runs to completion and
    /// bills the work anyway.
    #[must_use]
    pub fn timeout(
        &self,
        default_timeout: Duration,
        remaining_lifetime: Option<Duration>,
        run_deadline: Option<Duration>,
    ) -> Duration {
        let requested = self
            .timeout_secs
            .map_or(default_timeout, BashTimeoutSecs::duration);
        [remaining_lifetime, run_deadline]
            .into_iter()
            .flatten()
            .fold(requested, Duration::min)
    }
}

/// Returns the time left before `deadline`, or `None` when there is no deadline.
///
/// An already-passed deadline yields `Some(Duration::ZERO)` rather than `None`:
/// "the sandbox is out of time" and "the sandbox has no deadline" must not
/// collapse into the same value, or an expired sandbox would run unbounded.
#[must_use]
pub fn remaining_lifetime(deadline: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<Duration> {
    let deadline = deadline?;
    Some((deadline - now).to_std().unwrap_or(Duration::ZERO))
}

/// Refuses a call whose sandbox or run budget leaves no time to execute.
///
/// Starting a command with a zero budget would report a `timed out after 0s`
/// failure that reads like the command's own fault. The sandbox being out of
/// time is a different fact and is named as one.
fn reject_exhausted_budget(
    timeout: Duration,
    remaining_lifetime: Option<Duration>,
    run_deadline: Option<Duration>,
) -> Result<()> {
    if !timeout.is_zero() {
        return Ok(());
    }
    // Which clock ran out changes what the caller should do — provision a new
    // sandbox, or stop the run entirely — so the two are reported as different
    // errors rather than one ambiguous "out of time". The run deadline is
    // checked first: when both are exhausted, the run being over is the fact
    // that makes re-provisioning pointless.
    if run_deadline.is_some_and(|remaining| remaining.is_zero()) {
        return Err(MoaError::BudgetExhausted(
            "run deadline has passed; no time remains to run a command".to_string(),
        ));
    }
    if remaining_lifetime.is_some_and(|remaining| remaining.is_zero()) {
        return Err(MoaError::ToolError(
            "sandbox lifetime is exhausted; no time remains to run a command".to_string(),
        ));
    }
    // Neither clock is out, so a zero timeout means the deployment default was
    // itself zero. Blaming the sandbox here would send the caller to reprovision
    // a sandbox that is perfectly healthy.
    Err(MoaError::ValidationError(
        "resolved command timeout is zero with no exhausted deadline; \
         the configured default timeout is invalid"
            .to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{TimeDelta, Utc};
    use moa_core::error::MoaError;
    use moa_core::types::tools::ToolOutput;
    use tokio_util::sync::CancellationToken;

    use super::{
        BashToolInput, MAX_BASH_TIMEOUT_SECS, MAX_CAPTURED_STREAM_BYTES, execute_local,
        process_output, remaining_lifetime,
    };

    #[test]
    fn a_timeout_above_policy_is_rejected_rather_than_clamped() {
        // Pins: the schema's `maximum` is an advertisement; this is the bound.
        // A caller asking for a day gets a typed validation error naming the
        // limit, and — critically — no `BashToolInput` at all, so no executor
        // downstream can be handed a clamped-but-accepted value.
        let error = BashToolInput::parse(r#"{"cmd":"sleep 100000","timeout_secs":86400}"#)
            .expect_err("an out-of-policy timeout must not parse");
        match error {
            MoaError::ValidationError(message) => {
                assert!(message.contains("86400"), "names the request: {message}");
                assert!(
                    message.contains(&MAX_BASH_TIMEOUT_SECS.to_string()),
                    "names the limit: {message}"
                );
            }
            other => panic!("expected ValidationError, got {other:?}"),
        }

        // Exactly the bound is accepted; one second past it is not.
        assert_eq!(
            BashToolInput::parse(r#"{"cmd":"true","timeout_secs":300}"#)
                .expect("the exact bound is in policy")
                .timeout(Duration::from_secs(300), None, None),
            Duration::from_secs(300)
        );
        assert!(BashToolInput::parse(r#"{"cmd":"true","timeout_secs":301}"#).is_err());

        // Zero is not "no timeout": it would make every command fail instantly
        // while reading like a deliberate setting.
        assert!(BashToolInput::parse(r#"{"cmd":"true","timeout_secs":0}"#).is_err());
    }

    #[test]
    fn the_effective_timeout_is_the_smallest_of_request_default_and_sandbox_lifetime() {
        // Pins: an in-policy request still cannot outlive the sandbox it runs
        // in. A command started 30 seconds before the sandbox's hard deadline
        // gets 30 seconds, not the 300 it asked for, so the reaper never
        // destroys a sandbox out from under a running process.
        let params = BashToolInput::parse(r#"{"cmd":"true","timeout_secs":300}"#)
            .expect("an in-policy timeout parses");

        assert_eq!(
            params.timeout(
                Duration::from_secs(300),
                Some(Duration::from_secs(30)),
                None
            ),
            Duration::from_secs(30),
            "the sandbox lifetime is the binding constraint"
        );
        assert_eq!(
            params.timeout(Duration::from_secs(300), None, None),
            Duration::from_secs(300),
            "an unbounded-lifetime sandbox leaves the request in force"
        );

        // With no caller request the deployment default applies, and the
        // sandbox lifetime still caps it.
        let defaulted =
            BashToolInput::parse(r#"{"cmd":"true"}"#).expect("an absent timeout parses");
        assert_eq!(
            defaulted.timeout(Duration::from_secs(120), Some(Duration::from_secs(5)), None),
            Duration::from_secs(5)
        );
        assert_eq!(
            defaulted.timeout(Duration::from_secs(120), None, None),
            Duration::from_secs(120)
        );
    }

    #[test]
    fn an_expired_sandbox_reports_zero_remaining_rather_than_no_deadline() {
        // Pins: the two absent-looking states stay distinct. A sandbox past its
        // deadline yields `Some(ZERO)` — which `reject_exhausted_budget`
        // refuses — while an unbounded sandbox yields `None`, which does not.
        let now = Utc::now();
        assert_eq!(remaining_lifetime(None, now), None);
        assert_eq!(
            remaining_lifetime(Some(now - TimeDelta::seconds(5)), now),
            Some(Duration::ZERO)
        );
        assert_eq!(
            remaining_lifetime(Some(now + TimeDelta::seconds(45)), now),
            Some(Duration::from_secs(45)),
            "a future deadline yields exactly the time left"
        );
    }

    #[tokio::test]
    async fn binding_run_deadline_cancels_the_hard_token_and_process() {
        // Pins: the command's run-derived timeout cannot win a race and return
        // while leaving the shared hard token live. Expiry cancels the token
        // before the local child process is dropped.
        let sandbox = tempfile::tempdir().expect("temporary sandbox");
        let hard_cancel = CancellationToken::new();

        let error = tokio::time::timeout(
            Duration::from_secs(1),
            execute_local(
                sandbox.path(),
                r#"{"cmd":"sleep 30"}"#,
                Duration::from_secs(30),
                None,
                Some(Duration::from_millis(25)),
                Some(&hard_cancel),
            ),
        )
        .await
        .expect("the run deadline must stop the command")
        .expect_err("the command must not complete");

        assert!(matches!(error, MoaError::BudgetExhausted(_)));
        assert!(hard_cancel.is_cancelled());
    }

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
        assert!(output.process_stdout_truncated());
        assert!(!output.process_stderr_truncated());
    }
}

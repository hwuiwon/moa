//! Tool definition, policy, and output types.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use super::{
    action_policy::ActionClass, action_policy::ActionPolicyEffect, action_policy::RiskLevel,
    events_stream::ClaimCheck, hands::SandboxFile, identifiers::SessionId, identifiers::ToolCallId,
    security::ToolCapabilityId, security::ToolOutputAssessment,
};

fn default_tool_max_output_tokens() -> u32 {
    8_000
}

/// Addressable stream within a persisted tool-result artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ToolArtifactStream {
    /// Combined rendered tool output.
    Combined,
    /// Process stdout stream when available.
    Stdout,
    /// Process stderr stream when available.
    Stderr,
}

impl ToolArtifactStream {
    /// Returns the stable string form used in prompts and structured payloads.
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Durable reference to a large tool output persisted outside the event row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutputArtifact {
    /// Combined rendered tool output used for default reads and searches.
    pub combined: ClaimCheck,
    /// Approximate token count of the original persisted output.
    pub estimated_tokens: u32,
    /// Total number of lines in the combined output.
    pub line_count: usize,
    /// Byte range for stdout inside [`Self::combined`] for newly written artifacts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_range: Option<ToolArtifactByteRange>,
    /// Byte range for stderr inside [`Self::combined`] for newly written artifacts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_range: Option<ToolArtifactByteRange>,
    /// Legacy separately persisted stdout stream.
    ///
    /// New artifacts leave this empty and use [`Self::stdout_range`]. It remains
    /// readable so events written before the single-blob format continue to work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<ClaimCheck>,
    /// Legacy separately persisted stderr stream.
    ///
    /// New artifacts leave this empty and use [`Self::stderr_range`]. It remains
    /// readable so events written before the single-blob format continue to work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<ClaimCheck>,
}

/// A UTF-8 byte range into a combined tool-output artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolArtifactByteRange {
    /// Inclusive byte offset of the stream.
    pub start: usize,
    /// Exclusive byte offset of the stream.
    pub end: usize,
}

/// Failure while resolving a stream range from a combined artifact.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ToolArtifactSliceError {
    /// The persisted range is outside the loaded blob or is reversed.
    #[error("artifact range {start}..{end} is outside a {text_len}-byte blob")]
    OutOfBounds {
        /// Inclusive start offset.
        start: usize,
        /// Exclusive end offset.
        end: usize,
        /// Loaded blob length.
        text_len: usize,
    },
    /// A persisted offset would split a UTF-8 code point.
    #[error("artifact range offset {offset} is not a UTF-8 character boundary")]
    NotUtf8Boundary {
        /// Invalid byte offset.
        offset: usize,
    },
}

impl ToolArtifactByteRange {
    /// Slices a loaded combined artifact without copying or splitting UTF-8.
    pub fn slice<'a>(&self, text: &'a str) -> Result<&'a str, ToolArtifactSliceError> {
        if self.start > self.end || self.end > text.len() {
            return Err(ToolArtifactSliceError::OutOfBounds {
                start: self.start,
                end: self.end,
                text_len: text.len(),
            });
        }
        if !text.is_char_boundary(self.start) {
            return Err(ToolArtifactSliceError::NotUtf8Boundary { offset: self.start });
        }
        if !text.is_char_boundary(self.end) {
            return Err(ToolArtifactSliceError::NotUtf8Boundary { offset: self.end });
        }
        Ok(&text[self.start..self.end])
    }
}

/// Metadata for one trusted sandbox file stored in a durable manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedSandboxFileEntry {
    /// POSIX relative path inside the sandbox.
    pub path: String,
    /// SHA-256 hash of the raw file bytes, encoded as lowercase hex.
    pub content_sha256: String,
    /// Raw byte length of the file content.
    pub size: usize,
    /// Whether the file should be executable after installation.
    #[serde(default)]
    pub executable: bool,
}

/// Durable reference to trusted sandbox files selected for a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedSandboxFileManifestRef {
    /// Content-addressed blob id containing the serialized manifest payload.
    pub blob_id: String,
    /// Original serialized manifest size in bytes.
    pub size: usize,
    /// SHA-256 hash of the serialized manifest payload, encoded as lowercase hex.
    pub manifest_sha256: String,
    /// Per-file metadata used to validate the loaded manifest before installation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<TrustedSandboxFileEntry>,
}

/// Serialized payload stored behind a trusted sandbox file manifest reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedSandboxFileManifestPayload {
    /// Trusted files to materialize before hand tool execution.
    pub files: Vec<SandboxFile>,
}

impl ToolOutputArtifact {
    /// Returns the legacy claim check for one separately stored stream.
    ///
    /// New single-blob artifacts expose stdout and stderr through
    /// [`Self::stream_range`] and [`Self::slice_stream`].
    pub fn claim_check(&self, stream: ToolArtifactStream) -> Option<&ClaimCheck> {
        match stream {
            ToolArtifactStream::Combined => Some(&self.combined),
            ToolArtifactStream::Stdout => self.stdout.as_ref(),
            ToolArtifactStream::Stderr => self.stderr.as_ref(),
        }
    }

    /// Returns the range for a stream stored inside the combined blob.
    pub fn stream_range(&self, stream: ToolArtifactStream) -> Option<&ToolArtifactByteRange> {
        match stream {
            ToolArtifactStream::Combined => None,
            ToolArtifactStream::Stdout => self.stdout_range.as_ref(),
            ToolArtifactStream::Stderr => self.stderr_range.as_ref(),
        }
    }

    /// Resolves one stream from a loaded combined artifact without copying it.
    pub fn slice_stream<'a>(
        &self,
        stream: ToolArtifactStream,
        combined: &'a str,
    ) -> Result<Option<&'a str>, ToolArtifactSliceError> {
        match stream {
            ToolArtifactStream::Combined => Ok(Some(combined)),
            ToolArtifactStream::Stdout | ToolArtifactStream::Stderr => self
                .stream_range(stream)
                .map(|range| range.slice(combined))
                .transpose(),
        }
    }

    /// Returns the available stream names for prompting and diagnostics.
    pub fn available_streams(&self) -> Vec<&'static str> {
        let mut streams = vec![ToolArtifactStream::Combined.as_str()];
        if self.stdout_range.is_some() || self.stdout.is_some() {
            streams.push(ToolArtifactStream::Stdout.as_str());
        }
        if self.stderr_range.is_some() || self.stderr.is_some() {
            streams.push(ToolArtifactStream::Stderr.as_str());
        }
        streams
    }
}

/// Standard tool execution result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolContent {
    /// Plain-text tool output intended for humans or the LLM.
    Text {
        /// Text payload.
        text: String,
    },
    /// Structured JSON payload returned by a tool.
    Json {
        /// JSON payload.
        data: Value,
    },
    /// Raw streams and status from a process-backed tool.
    ///
    /// Process output is kept in this single carrier. The output's optional
    /// structured field does not contain copies of stdout or stderr.
    Process {
        /// Process streams and status.
        output: ProcessOutput,
    },
}

/// Raw streams and status returned by a process-backed tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessOutput {
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
    /// Process exit code.
    pub exit_code: i32,
    /// Whether stdout was truncated by the source adapter.
    #[serde(default)]
    pub stdout_truncated: bool,
    /// Whether stderr was truncated by the source adapter.
    #[serde(default)]
    pub stderr_truncated: bool,
}

impl ProcessOutput {
    /// Returns the stable display blocks for this process result.
    pub fn rendered_blocks(&self) -> Vec<String> {
        let mut blocks = Vec::new();
        if !self.stdout.is_empty() {
            blocks.push(self.stdout.clone());
        }
        if !self.stderr.is_empty() {
            blocks.push(format!("stderr:\n{}", self.stderr));
        }
        if blocks.is_empty() || self.exit_code != 0 {
            blocks.push(format!("exit_code: {}", self.exit_code));
        }
        blocks
    }

    /// Renders the process result using the stable tool-output text format.
    pub fn to_text(&self) -> String {
        self.rendered_blocks()
            .into_iter()
            .map(|block| block.trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

impl ToolContent {
    /// Renders one content block for provider adapters that need text input.
    pub fn rendered_text(&self) -> String {
        match self {
            Self::Text { text } => text.clone(),
            Self::Json { data } => data.to_string(),
            Self::Process { output } => output.to_text(),
        }
    }
}

/// High-level shape of tool inputs for normalization and action reviews.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolInputShape {
    /// Shell command input.
    Command,
    /// Filesystem path input.
    Path,
    /// Glob or pattern input.
    Pattern,
    /// Free-text query input.
    Query,
    /// URL input.
    Url,
    /// Structured JSON input.
    Json,
}

/// Strategy for rendering diffs during action reviews.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolDiffStrategy {
    /// No diff preview is available.
    None,
    /// The tool writes a full file body and can show a file diff.
    FileWrite,
    /// The tool replaces a single matched region and can show a surgical diff preview.
    StrReplace,
}

/// Replay and retry semantics declared for one tool definition.
///
/// A connector operation may additionally declare one reviewed upstream
/// idempotency header. That transport contract sends the durable tool-call ID;
/// this enum still describes the operation's effect semantics and does not, by
/// itself, authorize a caller-selected header or retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdempotencyClass {
    /// Safe to retry freely because the effect is deterministic for the same input.
    /// This is the only class the runtime automatically retries after uncertain execution.
    Idempotent,
    /// Unsafe to retry automatically because repeated execution may duplicate side effects.
    /// Automatic retry and route fallback are blocked once execution has begun.
    NonIdempotent,
}

/// Static action-policy metadata for a tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPolicySpec {
    /// Risk level assigned to this tool.
    pub risk_level: RiskLevel,
    /// Default effect when no config override or policy rule matches.
    pub default_effect: ActionPolicyEffect,
    /// Policy/audit class assigned to this tool.
    pub action_class: ActionClass,
    /// Input shape used for normalization and review summaries.
    pub input_shape: ToolInputShape,
    /// Diff strategy used for review previews.
    pub diff_strategy: ToolDiffStrategy,
}

/// Creates a read-only tool policy.
pub fn read_tool_policy(input_shape: ToolInputShape) -> ToolPolicySpec {
    ToolPolicySpec {
        risk_level: RiskLevel::Low,
        default_effect: ActionPolicyEffect::Allow,
        action_class: ActionClass::Read,
        input_shape,
        diff_strategy: ToolDiffStrategy::None,
    }
}

/// Creates a write-capable tool policy.
///
/// `pub` as a test seam: paired with [`read_tool_policy`] for cross-crate tool-registry tests.
pub fn write_tool_policy(
    input_shape: ToolInputShape,
    diff_strategy: ToolDiffStrategy,
) -> ToolPolicySpec {
    ToolPolicySpec {
        risk_level: RiskLevel::Medium,
        default_effect: ActionPolicyEffect::Allow,
        action_class: ActionClass::LocalWrite,
        input_shape,
        diff_strategy,
    }
}

/// Standard tool execution result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    /// Content blocks for human, UI, and LLM consumption.
    pub content: Vec<ToolContent>,
    /// Whether the tool result represents an error.
    pub is_error: bool,
    /// Optional additional structured payload for programmatic consumers.
    ///
    /// [`ToolOutput::json`] stores its canonical JSON payload in a
    /// [`ToolContent::Json`] block and leaves this field empty. Callers that
    /// need the machine-readable payload should use [`Self::structured_payload`]
    /// so both current and legacy serialized outputs are handled consistently.
    #[serde(default)]
    pub structured: Option<Value>,
    /// Execution duration.
    pub duration: Duration,
    /// Whether the tool output was truncated before storage or replay.
    #[serde(default)]
    pub truncated: bool,
    /// Approximate token count before router-level truncation, when truncation occurred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_output_tokens: Option<u32>,
    /// Durable artifact reference for oversized successful tool output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<ToolOutputArtifact>,
}

impl ToolOutput {
    /// Creates a successful text-only tool result.
    pub fn text(text: impl Into<String>, duration: Duration) -> Self {
        Self {
            content: vec![ToolContent::Text { text: text.into() }],
            is_error: false,
            structured: None,
            duration,
            truncated: false,
            original_output_tokens: None,
            artifact: None,
        }
    }

    /// Creates a process-backed tool result while preserving stdout, stderr, and exit code.
    pub fn from_process(
        stdout: String,
        stderr: String,
        exit_code: i32,
        duration: Duration,
    ) -> Self {
        Self::from_process_parts(stdout, stderr, exit_code, duration, false, false, None)
    }

    /// Creates a process-backed tool result whose raw stdout or stderr was
    /// truncated by the execution adapter before router-level budgeting.
    pub fn from_process_with_source_truncation(
        stdout: String,
        stderr: String,
        exit_code: i32,
        duration: Duration,
        stdout_truncated: bool,
        stderr_truncated: bool,
        original_output_tokens: Option<u32>,
    ) -> Self {
        Self::from_process_parts(
            stdout,
            stderr,
            exit_code,
            duration,
            stdout_truncated,
            stderr_truncated,
            original_output_tokens,
        )
    }

    fn from_process_parts(
        stdout: String,
        stderr: String,
        exit_code: i32,
        duration: Duration,
        stdout_truncated: bool,
        stderr_truncated: bool,
        original_output_tokens: Option<u32>,
    ) -> Self {
        Self {
            content: vec![ToolContent::Process {
                output: ProcessOutput {
                    stdout,
                    stderr,
                    exit_code,
                    stdout_truncated,
                    stderr_truncated,
                },
            }],
            is_error: exit_code != 0,
            structured: None,
            duration,
            truncated: stdout_truncated || stderr_truncated,
            original_output_tokens,
            artifact: None,
        }
    }

    /// Creates a successful structured JSON result with a text summary.
    pub fn json(summary: impl Into<String>, data: Value, duration: Duration) -> Self {
        Self {
            content: vec![
                ToolContent::Text {
                    text: summary.into(),
                },
                ToolContent::Json { data },
            ],
            is_error: false,
            structured: None,
            duration,
            truncated: false,
            original_output_tokens: None,
            artifact: None,
        }
    }

    /// Creates a text-only error result.
    pub fn error(message: impl Into<String>, duration: Duration) -> Self {
        Self {
            content: vec![ToolContent::Text {
                text: message.into(),
            }],
            is_error: true,
            structured: None,
            duration,
            truncated: false,
            original_output_tokens: None,
            artifact: None,
        }
    }

    /// Marks this tool output as truncated or untruncated.
    #[must_use]
    pub fn with_truncated(mut self, truncated: bool) -> Self {
        self.truncated = truncated;
        self
    }

    /// Records the approximate token count before truncation.
    #[must_use]
    pub fn with_original_output_tokens(mut self, original_output_tokens: Option<u32>) -> Self {
        self.original_output_tokens = original_output_tokens;
        self
    }

    /// Attaches a durable artifact reference for oversized successful output.
    #[must_use]
    pub fn with_artifact(mut self, artifact: Option<ToolOutputArtifact>) -> Self {
        self.artifact = artifact;
        self
    }

    /// Returns the canonical structured payload for this output.
    ///
    /// New JSON outputs carry their payload in a [`ToolContent::Json`] block;
    /// outputs with a separate machine payload retain it in `structured`.
    /// The latter fallback also lets older persisted outputs replay after the
    /// JSON constructor stopped storing the same value twice.
    pub fn structured_payload(&self) -> Option<&Value> {
        self.structured.as_ref().or_else(|| {
            self.content.iter().find_map(|block| match block {
                ToolContent::Json { data } => Some(data),
                _ => None,
            })
        })
    }

    fn process_output(&self) -> Option<&ProcessOutput> {
        self.content.iter().find_map(|block| match block {
            ToolContent::Process { output } => Some(output),
            _ => None,
        })
    }

    /// Returns the preserved process exit code when this output came from a shell-like tool.
    pub fn process_exit_code(&self) -> Option<i32> {
        if let Some(output) = self.process_output() {
            return Some(output.exit_code);
        }
        self.structured
            .as_ref()
            .and_then(|data| data.get("exit_code"))
            .and_then(Value::as_i64)
            .map(|value| value as i32)
    }

    /// Returns the preserved process stdout when this output came from a shell-like tool.
    pub fn process_stdout(&self) -> Option<&str> {
        if let Some(output) = self.process_output() {
            return Some(output.stdout.as_str());
        }
        self.structured
            .as_ref()
            .and_then(|data| data.get("stdout"))
            .and_then(Value::as_str)
    }

    /// Returns the preserved process stderr when this output came from a shell-like tool.
    pub fn process_stderr(&self) -> Option<&str> {
        if let Some(output) = self.process_output() {
            return Some(output.stderr.as_str());
        }
        self.structured
            .as_ref()
            .and_then(|data| data.get("stderr"))
            .and_then(Value::as_str)
    }

    /// Returns whether the source adapter truncated process stdout.
    pub fn process_stdout_truncated(&self) -> bool {
        self.process_output()
            .map(|output| output.stdout_truncated)
            .or_else(|| {
                self.structured
                    .as_ref()
                    .and_then(|data| data.get("stdout_truncated"))
                    .and_then(Value::as_bool)
            })
            .unwrap_or(false)
    }

    /// Returns whether the source adapter truncated process stderr.
    pub fn process_stderr_truncated(&self) -> bool {
        self.process_output()
            .map(|output| output.stderr_truncated)
            .or_else(|| {
                self.structured
                    .as_ref()
                    .and_then(|data| data.get("stderr_truncated"))
                    .and_then(Value::as_bool)
            })
            .unwrap_or(false)
    }

    /// Renders the tool result into a single text block suitable for the LLM context.
    pub fn to_text(&self) -> String {
        let rendered = self
            .content
            .iter()
            .map(|block| match block {
                ToolContent::Text { text } => text.trim_end().to_string(),
                ToolContent::Json { data } => {
                    serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string())
                }
                ToolContent::Process { output } => output.to_text(),
            })
            .filter(|block| !block.trim().is_empty())
            .collect::<Vec<_>>();

        if rendered.is_empty() {
            if self.is_error {
                "tool returned an error with no details".to_string()
            } else {
                "tool completed with no output".to_string()
            }
        } else {
            rendered.join("\n\n")
        }
    }
}

/// The only shape a classified tool output may travel in.
///
/// Every router and executor API returns this envelope rather than a bare
/// [`ToolOutput`]: the safe output, the assessment that produced it, and the
/// canonical capability identity the circuit is keyed by are inseparable. A
/// consumer therefore cannot obtain output without also obtaining the security
/// metadata, and no downstream surface ever re-runs the classifier.
///
/// `safe_output` is the *post-classification* output. Suspicious spans are
/// already redacted, and for every non-safe class the structured payload and
/// artifact reference are already cleared, so persisting or rendering it needs
/// no further sanitization.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecuredToolOutput {
    /// Post-classification output that is safe to persist, render, and replay.
    pub safe_output: ToolOutput,
    /// Required security metadata describing how the output was classified.
    pub assessment: ToolOutputAssessment,
    /// Canonical capability identity resolved by the router.
    pub capability: ToolCapabilityId,
    /// Sandbox hand that served the call, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hand_id: Option<String>,
}

impl SecuredToolOutput {
    /// Wraps an output that the classifier assessed as safe.
    ///
    /// Test and construction seam only: production output reaches this shape
    /// through the classifier in `moa-security`, never by asserting safety here.
    #[must_use]
    pub fn assessed_safe(safe_output: ToolOutput, capability: ToolCapabilityId) -> Self {
        Self {
            safe_output,
            assessment: ToolOutputAssessment::safe(),
            capability,
            hand_id: None,
        }
    }

    /// Attaches the sandbox hand that served the call.
    #[must_use]
    pub fn with_hand_id(mut self, hand_id: Option<String>) -> Self {
        self.hand_id = hand_id;
        self
    }

    /// Returns whether the underlying output represents a tool error.
    #[must_use]
    pub fn is_error(&self) -> bool {
        self.safe_output.is_error
    }
}

/// Durable request envelope for one tool execution routed through `ToolExecutor`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRequest {
    /// Stable MOA tool-call identifier used for event-log correlation and replay.
    pub tool_call_id: ToolCallId,
    /// Exact authenticated caller and delegation provenance admitted for this call.
    pub caller_identity: crate::traits::Identity,
    /// Provider-issued tool-use identifier when the request originated from an LLM turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_tool_use_id: Option<String>,
    /// Stable registered tool name.
    pub tool_name: String,
    /// Contract revision the caller was admitted against.
    ///
    /// Conversational calls pin the revision offered to the model, and durable
    /// calls pin the revision compiled into the execution capability.
    /// `ToolExecutor` compares this with the exact immutable catalog snapshot it
    /// uses for retry selection and dispatch.
    pub expected_tool_contract_revision: String,
    /// Raw JSON input passed to the tool implementation.
    pub input: Value,
    /// Active per-turn canary that must not appear in tool input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_canary: Option<String>,
    /// Exact persisted session that owns the durable tool call.
    pub session_id: SessionId,
    /// Durable trusted sandbox file manifest selected during context compilation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_sandbox_manifest: Option<TrustedSandboxFileManifestRef>,
    /// Worker hand scope that isolates this call's sandbox from the
    /// session-level coordinator scope. `None` keys the hand at the session
    /// level (the coordinator/root path); `Some(id)` keys it at
    /// `{session_id}:{worker_id}` so each worker owns its own sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    /// Downward-only resource slice that bounds this tool dispatch.
    #[serde(default)]
    pub resource_budget: crate::types::resource::ResourceBudget,
}

/// Name of the built-in tool that reads a line range from a stored tool result.
pub const TOOL_RESULT_READ_TOOL_NAME: &str = "tool_result_read";
/// Name of the built-in tool that searches a stored tool result.
pub const TOOL_RESULT_SEARCH_TOOL_NAME: &str = "tool_result_search";

/// Tools the agent loop needs in order to operate on its own outputs.
///
/// These are control, not capability: they do no task work, they are how the
/// model recovers content from a tool result that was truncated or claim-checked
/// out of the transcript. A loadout that drops them still looks complete while
/// every large tool output silently becomes unreadable, so a selection policy
/// that must fit a schema cap keeps them ahead of anything it is choosing
/// between.
pub const CONTROL_TOOL_NAMES: [&str; 2] =
    [TOOL_RESULT_READ_TOOL_NAME, TOOL_RESULT_SEARCH_TOOL_NAME];

/// Shared metadata that describes one callable tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Stable tool name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON schema for parameters.
    pub schema: Value,
    /// Static action-policy metadata.
    pub policy: ToolPolicySpec,
    /// Declared retry/idempotency semantics for the tool implementation.
    pub idempotency_class: IdempotencyClass,
    /// Exact source-owned declaration of the governed tool that reverses this effect.
    pub rollback: Option<ToolRollbackDefinition>,
    /// Approximate maximum output tokens persisted for one successful call.
    #[serde(default = "default_tool_max_output_tokens")]
    pub max_output_tokens: u32,
}

/// Source-owned promise that one registered governed tool exactly reverses another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolRollbackDefinition {
    /// Stable registered name of the compensating governed tool.
    pub compensator_tool_name: String,
    /// Bounded deterministic mapping from committed forward data to compensator input.
    pub input_mapping: ToolRollbackInputMapping,
}

/// Source-level rollback input mapping resolved to exact catalog references at projection time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolRollbackInputMapping {
    /// Ordered bindings that construct the compensator input object.
    pub bindings: Vec<ToolRollbackInputBinding>,
}

/// One target field populated from the committed forward input or output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolRollbackInputBinding {
    /// RFC 6901 pointer in the compensator input schema.
    pub target_pointer: String,
    /// Exact committed value copied into the target.
    pub source: ToolRollbackValueSource,
}

/// Closed committed-data sources available to a registered tool rollback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolRollbackValueSource {
    /// Copy a value from the original forward input.
    OriginalInput {
        /// RFC 6901 pointer, or the empty string for the complete input.
        pointer: String,
    },
    /// Copy a value from the committed forward output.
    OriginalOutput {
        /// RFC 6901 pointer, or the empty string for the complete output.
        pointer: String,
    },
}

impl ToolDefinition {
    /// Converts the definition into the Anthropic tool schema shape.
    pub fn anthropic_schema(&self) -> Value {
        serde_json::json!({
            "name": self.name,
            "description": self.description,
            "input_schema": self.schema,
        })
    }
}

/// Normalized policy-facing description of one tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPolicyInput {
    /// Tool name being invoked.
    pub tool_name: String,
    /// Normalized string used for rule matching.
    pub normalized_input: String,
    /// Concise human-readable input summary.
    pub input_summary: String,
    /// Risk level assigned by the tool definition.
    pub risk_level: RiskLevel,
    /// Default effect when no config override or persisted rule matches.
    pub default_effect: ActionPolicyEffect,
    /// Policy/audit class assigned by the tool definition.
    pub action_class: ActionClass,
}

/// Owner that decided the effect of one tool-policy evaluation.
///
/// This is a closed, safe vocabulary: it names *which* authority produced the
/// outcome so decisions can be logged and audited without ever carrying the
/// invocation input, the matched glob pattern, or any credential material.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionPolicyDecisionSource {
    /// No rule or override applied; the deployment permission default decided.
    DeploymentDefault,
    /// No rule or override applied; the tool definition's intrinsic default decided.
    ToolDefinition,
    /// A persisted tenant or contact rule decided.
    PersistedRule,
    /// A configured admin-review override decided.
    ConfiguredReview,
    /// A configured always-deny override decided.
    ConfiguredDeny,
}

impl ActionPolicyDecisionSource {
    /// Returns the stable log/audit name for this decision source.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeploymentDefault => "deployment_default",
            Self::ToolDefinition => "tool_definition",
            Self::PersistedRule => "persisted_rule",
            Self::ConfiguredReview => "configured_review",
            Self::ConfiguredDeny => "configured_deny",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use uuid::Uuid;

    use super::{
        ClaimCheck, ToolArtifactByteRange, ToolArtifactStream, ToolCallRequest, ToolContent,
        ToolOutput,
    };
    use crate::{
        traits::{Identity, IdentityType},
        types::identifiers::{SessionId, TenantId, ToolCallId},
    };

    #[test]
    fn tool_call_request_requires_persisted_session_and_contract_identity() {
        // Pins: every durable tool execution names its exact session and admitted
        // catalog contract on the wire.
        let request = ToolCallRequest {
            tool_call_id: ToolCallId::new(),
            caller_identity: Identity {
                identity_type: IdentityType::Operator,
                id: Uuid::now_v7(),
                tenant_id: TenantId::new(),
                api_key_id: None,
                acting_on_behalf_of: None,
            },
            provider_tool_use_id: None,
            tool_name: "memory_search".to_string(),
            expected_tool_contract_revision: "contract-v1".to_string(),
            input: serde_json::json!({}),
            active_canary: None,
            session_id: SessionId::new(),
            trusted_sandbox_manifest: None,
            worker_id: None,
            resource_budget: Default::default(),
        };
        let mut missing_session = serde_json::to_value(request.clone()).expect("serialize request");
        missing_session
            .as_object_mut()
            .expect("tool request should serialize as an object")
            .remove("session_id");

        let error = serde_json::from_value::<ToolCallRequest>(missing_session)
            .expect_err("missing persisted session id must fail decoding");

        assert!(error.to_string().contains("missing field `session_id`"));

        let mut missing_contract = serde_json::to_value(request).expect("serialize request");
        missing_contract
            .as_object_mut()
            .expect("tool request should serialize as an object")
            .remove("expected_tool_contract_revision");

        let error = serde_json::from_value::<ToolCallRequest>(missing_contract)
            .expect_err("missing admitted contract must fail decoding");

        assert!(
            error
                .to_string()
                .contains("missing field `expected_tool_contract_revision`")
        );
    }

    #[test]
    fn tool_output_text_creates_single_text_block() {
        let output = ToolOutput::text("hello", Duration::from_millis(5));

        assert!(!output.is_error);
        assert_eq!(
            output.content,
            vec![ToolContent::Text {
                text: "hello".to_string()
            }]
        );
        assert!(!output.truncated);
        assert_eq!(output.to_text(), "hello");
    }

    #[test]
    fn tool_output_from_process_success_preserves_stdout() {
        let output = ToolOutput::from_process(
            "hello\n".to_string(),
            String::new(),
            0,
            Duration::from_millis(1),
        );

        assert!(!output.is_error);
        assert!(!output.truncated);
        assert_eq!(output.process_exit_code(), Some(0));
        assert_eq!(output.process_stdout(), Some("hello\n"));
        assert_eq!(output.to_text(), "hello");
    }

    #[test]
    fn process_output_has_one_canonical_stream_carrier() {
        // Pins: process streams are serialized once in ProcessOutput rather than
        // being copied into both display content and a structured JSON value.
        let output = ToolOutput::from_process(
            "hello\n".to_string(),
            "warning\n".to_string(),
            0,
            Duration::from_millis(1),
        );

        assert_eq!(output.content.len(), 1);
        let ToolContent::Process { output: process } = &output.content[0] else {
            panic!("process output should have one process content carrier");
        };
        assert_eq!(process.stdout, "hello\n");
        assert_eq!(process.stderr, "warning\n");
        assert!(output.structured.is_none());
        assert!(output.structured_payload().is_none());

        let encoded = serde_json::to_value(&output).expect("serialize process output");
        assert!(encoded["structured"].is_null());
        let encoded_text = encoded.to_string();
        assert_eq!(encoded_text.matches("hello\\n").count(), 1);
        assert_eq!(encoded_text.matches("warning\\n").count(), 1);

        let replayed: ToolOutput = serde_json::from_value(encoded).expect("replay process output");
        assert_eq!(replayed.process_stdout(), Some("hello\n"));
        assert_eq!(replayed.process_stderr(), Some("warning\n"));
        assert_eq!(replayed.process_exit_code(), Some(0));
        assert_eq!(replayed.to_text(), "hello\n\nstderr:\nwarning");
    }

    #[test]
    fn tool_output_from_process_failure_includes_exit_code_and_stderr() {
        let output = ToolOutput::from_process(
            "partial".to_string(),
            "boom".to_string(),
            7,
            Duration::from_millis(2),
        );

        assert!(output.is_error);
        assert!(!output.truncated);
        assert_eq!(output.process_exit_code(), Some(7));
        assert_eq!(output.process_stderr(), Some("boom"));
        assert!(output.to_text().contains("stderr:\nboom"));
        assert!(output.to_text().contains("exit_code: 7"));
    }

    #[test]
    fn tool_output_json_creates_text_and_json_blocks() {
        let data = serde_json::json!([{ "path": "a.txt" }]);
        let output = ToolOutput::json("2 matches", data.clone(), Duration::from_millis(3));

        assert!(!output.is_error);
        assert!(matches!(output.content[0], ToolContent::Text { .. }));
        assert!(matches!(output.content[1], ToolContent::Json { .. }));
        assert!(output.structured.is_none());
        assert_eq!(output.structured_payload(), Some(&data));
        assert!(!output.truncated);
        assert!(output.to_text().contains("2 matches"));
        assert!(output.to_text().contains("\"path\": \"a.txt\""));
    }

    #[test]
    fn legacy_structured_process_output_still_replays() {
        // Pins: rows written before ProcessOutput became the canonical carrier
        // still expose the process accessors during replay.
        let mut encoded = serde_json::to_value(ToolOutput::text("hello", Duration::from_millis(1)))
            .expect("serialize legacy-shaped output");
        encoded["structured"] = serde_json::json!({
            "stdout": "hello\n",
            "stderr": "",
            "exit_code": 0,
            "stdout_truncated": false,
            "stderr_truncated": false,
        });

        let replayed: ToolOutput = serde_json::from_value(encoded).expect("replay legacy output");
        assert_eq!(replayed.process_stdout(), Some("hello\n"));
        assert_eq!(replayed.process_stderr(), Some(""));
        assert_eq!(replayed.process_exit_code(), Some(0));
        assert!(!replayed.process_stdout_truncated());
        assert!(!replayed.process_stderr_truncated());
    }

    #[test]
    fn tool_output_error_sets_error_flag() {
        let output = ToolOutput::error("failed", Duration::from_secs(1));

        assert!(output.is_error);
        assert!(!output.truncated);
        assert_eq!(output.to_text(), "failed");
    }

    #[test]
    fn tool_output_artifact_streams_report_available_entries() {
        let artifact = super::ToolOutputArtifact {
            combined: ClaimCheck {
                blob_id: "combined".to_string(),
                size: 12,
                preview: "hello".to_string(),
            },
            estimated_tokens: 10,
            line_count: 3,
            stdout_range: None,
            stderr_range: None,
            stdout: Some(ClaimCheck {
                blob_id: "stdout".to_string(),
                size: 5,
                preview: "out".to_string(),
            }),
            stderr: None,
        };

        assert_eq!(
            artifact.available_streams(),
            vec![
                ToolArtifactStream::Combined.as_str(),
                ToolArtifactStream::Stdout.as_str()
            ]
        );
        assert_eq!(
            artifact
                .claim_check(ToolArtifactStream::Stdout)
                .expect("stdout claim check")
                .blob_id,
            "stdout"
        );
        assert!(artifact.claim_check(ToolArtifactStream::Stderr).is_none());
    }

    #[test]
    fn single_blob_stream_ranges_slice_unicode_safely() {
        let combined = "α\nβ\nstderr:\n警告\n";
        let stdout_end = "α\nβ".len();
        let stderr_start = combined.find("警告").expect("stderr text");
        let artifact = super::ToolOutputArtifact {
            combined: ClaimCheck {
                blob_id: "combined".to_string(),
                size: combined.len(),
                preview: combined.to_string(),
            },
            estimated_tokens: 4,
            line_count: 4,
            stdout_range: Some(ToolArtifactByteRange {
                start: 0,
                end: stdout_end,
            }),
            stderr_range: Some(ToolArtifactByteRange {
                start: stderr_start,
                end: stderr_start + "警告".len(),
            }),
            stdout: None,
            stderr: None,
        };

        assert_eq!(
            artifact
                .slice_stream(ToolArtifactStream::Stdout, combined)
                .expect("stdout range"),
            Some("α\nβ")
        );
        assert_eq!(
            artifact
                .slice_stream(ToolArtifactStream::Stderr, combined)
                .expect("stderr range"),
            Some("警告")
        );
        assert_eq!(
            artifact
                .slice_stream(ToolArtifactStream::Combined, combined)
                .expect("combined range"),
            Some(combined)
        );

        let invalid = super::ToolArtifactByteRange { start: 1, end: 2 };
        assert!(invalid.slice(combined).is_err());
    }
}

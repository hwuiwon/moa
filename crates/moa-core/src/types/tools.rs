//! Tool definition, policy, and output types.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    ActionClass, ActionPolicyEffect, ClaimCheck, RiskLevel, SandboxFile, SessionId, TenantId,
    ToolCallId, UserId,
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
    /// Persisted stdout stream for process-backed tools when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<ClaimCheck>,
    /// Persisted stderr stream for process-backed tools when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<ClaimCheck>,
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
    /// Returns the claim check for one artifact stream when present.
    pub fn claim_check(&self, stream: ToolArtifactStream) -> Option<&ClaimCheck> {
        match stream {
            ToolArtifactStream::Combined => Some(&self.combined),
            ToolArtifactStream::Stdout => self.stdout.as_ref(),
            ToolArtifactStream::Stderr => self.stderr.as_ref(),
        }
    }

    /// Returns the available stream names for prompting and diagnostics.
    pub fn available_streams(&self) -> Vec<&'static str> {
        let mut streams = vec![ToolArtifactStream::Combined.as_str()];
        if self.stdout.is_some() {
            streams.push(ToolArtifactStream::Stdout.as_str());
        }
        if self.stderr.is_some() {
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
/// There is deliberately no keyed-idempotency class: the runtime cannot honor
/// keyed idempotency end to end (a durable key is not threaded through the tool
/// invocation and hands-recovery boundaries), so promising it in the type would
/// let callers rely on behavior that is never enforced. Reintroduce a keyed class
/// only once a real consumer threads the key through invocation and recovery.
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
    /// Optional structured payload for programmatic consumers.
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
        let mut content = Vec::new();
        if !stdout.is_empty() {
            content.push(ToolContent::Text {
                text: stdout.clone(),
            });
        }
        if !stderr.is_empty() {
            content.push(ToolContent::Text {
                text: format!("stderr:\n{stderr}"),
            });
        }
        if content.is_empty() || exit_code != 0 {
            content.push(ToolContent::Text {
                text: format!("exit_code: {exit_code}"),
            });
        }

        Self {
            content,
            is_error: exit_code != 0,
            structured: Some(serde_json::json!({
                "stdout": stdout,
                "stderr": stderr,
                "exit_code": exit_code,
                "stdout_truncated": stdout_truncated,
                "stderr_truncated": stderr_truncated,
            })),
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
                ToolContent::Json { data: data.clone() },
            ],
            is_error: false,
            structured: Some(data),
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

    /// Returns the preserved process exit code when this output came from a shell-like tool.
    pub fn process_exit_code(&self) -> Option<i32> {
        self.structured
            .as_ref()
            .and_then(|data| data.get("exit_code"))
            .and_then(Value::as_i64)
            .map(|value| value as i32)
    }

    /// Returns the preserved process stdout when this output came from a shell-like tool.
    pub fn process_stdout(&self) -> Option<&str> {
        self.structured
            .as_ref()
            .and_then(|data| data.get("stdout"))
            .and_then(Value::as_str)
    }

    /// Returns the preserved process stderr when this output came from a shell-like tool.
    pub fn process_stderr(&self) -> Option<&str> {
        self.structured
            .as_ref()
            .and_then(|data| data.get("stderr"))
            .and_then(Value::as_str)
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

/// Durable request envelope for one tool execution routed through `ToolExecutor`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRequest {
    /// Stable MOA tool-call identifier used for event-log correlation and replay.
    pub tool_call_id: ToolCallId,
    /// Provider-issued tool-use identifier when the request originated from an LLM turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_tool_use_id: Option<String>,
    /// Stable registered tool name.
    pub tool_name: String,
    /// Raw JSON input passed to the tool implementation.
    pub input: Value,
    /// Active per-turn canary that must not appear in tool input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_canary: Option<String>,
    /// Owning session when the tool call is part of a durable MOA turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    /// Tenant scope used when the call is executed without a persisted session.
    pub tenant_id: TenantId,
    /// User scope used when the call is executed without a persisted session.
    pub user_id: UserId,
    /// Durable trusted sandbox file manifest selected during context compilation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_sandbox_manifest: Option<TrustedSandboxFileManifestRef>,
    /// Worker hand scope that isolates this call's sandbox from the
    /// session-level coordinator scope. `None` keys the hand at the session
    /// level (the coordinator/root path); `Some(id)` keys it at
    /// `{session_id}:{worker_id}` so each worker owns its own sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
}

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
    /// Approximate maximum output tokens persisted for one successful call.
    #[serde(default = "default_tool_max_output_tokens")]
    pub max_output_tokens: u32,
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{ClaimCheck, ToolArtifactStream, ToolContent, ToolOutput};

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
        let output = ToolOutput::json(
            "2 matches",
            serde_json::json!([{ "path": "a.txt" }]),
            Duration::from_millis(3),
        );

        assert!(!output.is_error);
        assert!(matches!(output.content[0], ToolContent::Text { .. }));
        assert!(matches!(output.content[1], ToolContent::Json { .. }));
        assert!(!output.truncated);
        assert!(output.to_text().contains("2 matches"));
        assert!(output.to_text().contains("\"path\": \"a.txt\""));
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
}

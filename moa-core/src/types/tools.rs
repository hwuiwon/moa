//! Tool definition, policy, and output types.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{PolicyAction, RiskLevel};

fn default_tool_max_output_tokens() -> u32 {
    8_000
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

/// High-level shape of tool inputs for normalization and approvals.
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

/// Strategy for rendering diffs during approvals.
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

/// Static policy and approval metadata for a tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPolicySpec {
    /// Risk level shown to the user for this tool.
    pub risk_level: RiskLevel,
    /// Default action when no config override or approval rule matches.
    pub default_action: PolicyAction,
    /// Input shape used for normalization and approval summaries.
    pub input_shape: ToolInputShape,
    /// Diff strategy used for approval previews.
    pub diff_strategy: ToolDiffStrategy,
}

/// Creates a read-only tool policy with auto-approval.
pub fn read_tool_policy(input_shape: ToolInputShape) -> ToolPolicySpec {
    ToolPolicySpec {
        risk_level: RiskLevel::Low,
        default_action: PolicyAction::Allow,
        input_shape,
        diff_strategy: ToolDiffStrategy::None,
    }
}

/// Creates a write-capable tool policy that requires approval.
pub fn write_tool_policy(
    input_shape: ToolInputShape,
    diff_strategy: ToolDiffStrategy,
) -> ToolPolicySpec {
    ToolPolicySpec {
        risk_level: RiskLevel::Medium,
        default_action: PolicyAction::RequireApproval,
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
        }
    }

    /// Creates a process-backed tool result while preserving stdout, stderr, and exit code.
    pub fn from_process(
        stdout: String,
        stderr: String,
        exit_code: i32,
        duration: Duration,
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
            })),
            duration,
            truncated: false,
            original_output_tokens: None,
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

/// Shared metadata that describes one callable tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Stable tool name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON schema for parameters.
    pub schema: Value,
    /// Static policy and approval metadata.
    pub policy: ToolPolicySpec,
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
    /// Default action when no config override or persisted rule matches.
    pub default_action: PolicyAction,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{ToolContent, ToolOutput};

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
    fn tool_output_roundtrips_through_json() {
        let output = ToolOutput::json(
            "1 match",
            serde_json::json!({ "path": "notes.md" }),
            Duration::from_millis(4),
        );

        let encoded = serde_json::to_string(&output).expect("serialize tool output");
        let decoded: ToolOutput = serde_json::from_str(&encoded).expect("deserialize tool output");

        assert_eq!(decoded, output);
    }
}

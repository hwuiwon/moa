//! Tool definition, policy, and output types.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
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

/// Catalog-pinned provider completion mode for one governed tool contract.
///
/// External-job capacity is reserved before provider dispatch only for tools
/// that explicitly opt into asynchronous completion. Returning an external job
/// from a synchronous-only contract is an invariant violation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolAsyncMode {
    /// Every admitted invocation completes within the bounded provider call.
    SynchronousOnly,
    /// An admitted invocation may commit a provider-owned asynchronous job.
    MayReturnExternalJob {
        /// Registered adapter/provider key that owns start recovery and callbacks.
        provider: String,
    },
}

/// Exact pre-provider identity an asynchronous-capable tool must use for start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalJobStartContext {
    /// MOA-owned job identity reserved before any provider network call.
    pub external_job_uid: uuid::Uuid,
    /// Catalog-pinned adapter/provider key.
    pub provider: String,
    /// Deterministic provider idempotency key used by start and recovery.
    pub idempotency_key: String,
}

/// Provider-owned asynchronous job returned after a capability has committed its start.
///
/// MOA assigns its own durable external-job UID when this outcome is persisted.
/// These fields are the immutable provider identity and recovery contract needed
/// to authenticate callbacks, reconcile sparsely, and cancel without keeping a
/// workflow or sandbox active.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AsyncToolJob {
    /// Stable provider implementation name.
    pub provider: String,
    /// Provider-issued external job identity.
    pub provider_job_id: String,
    /// Stable provider idempotency key used for start, reconciliation, and cancel.
    pub idempotency_key: String,
    /// Vault or connection reference used to authenticate provider callbacks.
    pub callback_auth_reference: String,
    /// Latest bounded provider progress phase.
    pub progress_phase: String,
    /// Whether the provider exposes definitive cancellation.
    pub cancel_supported: bool,
    /// Earliest time at which sparse provider reconciliation may run.
    pub next_reconcile_at: DateTime<Utc>,
}

/// Terminal provider outcome carried by an authenticated asynchronous callback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum AsyncToolJobTerminalOutcome {
    /// Provider work completed with structured output.
    Completed {
        /// Provider result validated by the capability adapter.
        output: Value,
    },
    /// Provider work failed definitively.
    Failed {
        /// Structured provider failure evidence.
        error: Value,
    },
    /// Provider work was cancelled definitively.
    Cancelled,
    /// The provider effect may have committed but cannot be determined safely.
    UnknownOutcome {
        /// Structured ambiguity evidence for operator resolution.
        error: Value,
    },
}

/// Authenticated provider event accepted for one exact asynchronous-job generation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum AsyncToolJobCallbackOutcome {
    /// The provider reports durable nonterminal progress.
    Progress {
        /// Latest bounded provider progress phase.
        progress_phase: String,
        /// Earliest time at which sparse provider reconciliation may run.
        next_reconcile_at: DateTime<Utc>,
    },
    /// The provider reports a definitive or explicitly ambiguous terminal outcome.
    Terminal {
        /// Typed terminal provider outcome.
        outcome: AsyncToolJobTerminalOutcome,
    },
}

/// Result of requesting cancellation for one exact provider-job generation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum AsyncToolJobCancelOutcome {
    /// Provider confirmed terminal cancellation.
    Cancelled,
    /// Provider accepted cancellation and requires later callback or reconciliation.
    Accepted {
        /// Earliest sparse reconciliation time.
        next_reconcile_at: DateTime<Utc>,
        /// Latest provider progress phase.
        progress_phase: String,
    },
    /// Provider does not support cancellation for this job.
    Unsupported,
    /// Cancellation transport completed ambiguously.
    UnknownOutcome {
        /// Structured ambiguity evidence for operator resolution.
        error: Value,
    },
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
#[derive(Debug, Clone, PartialEq)]
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
    pub structured: Option<Value>,
    /// Execution duration.
    pub duration: Duration,
    /// Whether the tool output was truncated before storage or replay.
    pub truncated: bool,
    /// Approximate token count before router-level truncation, when truncation occurred.
    pub original_output_tokens: Option<u32>,
    /// Durable artifact reference for oversized tool output, including failures.
    pub artifact: Option<ToolOutputArtifact>,
}

/// Owned durable shape accepted from current, parent, and transient readers.
#[derive(Deserialize)]
struct ToolOutputWire {
    content: Vec<ToolContentWire>,
    is_error: bool,
    #[serde(default)]
    structured: Option<Value>,
    duration: Duration,
    #[serde(default)]
    truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    original_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    artifact: Option<ToolOutputArtifact>,
}

/// Tool content accepted on the durable wire.
///
/// `Process` remains decodable because the immediately preceding revision may
/// already have journaled it. New writes use only the parent `Text` and `Json`
/// variants through [`ToolOutput`]'s borrowed serializer.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ToolContentWire {
    Text { text: String },
    Json { data: Value },
    Process { output: ProcessOutput },
}

/// Borrowed durable shape emitted for retained parent readers.
#[derive(Serialize)]
struct ToolOutputWireRef<'a> {
    content: ToolContentWireRef<'a>,
    is_error: bool,
    structured: Option<StructuredWireRef<'a>>,
    duration: &'a Duration,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    original_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    artifact: Option<&'a ToolOutputArtifact>,
}

/// Borrowed content or the only owned compatibility projection: process text.
enum ToolContentWireRef<'a> {
    Borrowed(&'a [ToolContent]),
    RenderedProcess(&'a [String]),
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ToolContentBlockWireRef<'a> {
    Text { text: &'a str },
    Json { data: &'a Value },
}

#[derive(Serialize)]
#[serde(untagged)]
enum StructuredWireRef<'a> {
    Json(&'a Value),
    Process(&'a ProcessOutput),
}

impl Serialize for ToolContentWireRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let len = match self {
            Self::Borrowed(content) => content.len(),
            Self::RenderedProcess(content) => content.len(),
        };
        let mut sequence = serializer.serialize_seq(Some(len))?;
        match self {
            Self::Borrowed(content) => {
                for block in *content {
                    let block = match block {
                        ToolContent::Text { text } => ToolContentBlockWireRef::Text { text },
                        ToolContent::Json { data } => ToolContentBlockWireRef::Json { data },
                        ToolContent::Process { .. } => {
                            return Err(serde::ser::Error::custom(NONCANONICAL_PROCESS_WIRE_ERROR));
                        }
                    };
                    sequence.serialize_element(&block)?;
                }
            }
            Self::RenderedProcess(content) => {
                for text in *content {
                    sequence.serialize_element(&ToolContentBlockWireRef::Text { text })?;
                }
            }
        }
        sequence.end()
    }
}

const NONCANONICAL_PROCESS_WIRE_ERROR: &str =
    "process ToolOutput must contain exactly one process block and no structured or JSON carrier";

impl Serialize for ToolOutput {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let process = self.process_for_wire().map_err(serde::ser::Error::custom)?;
        let rendered_process = process.map(ProcessOutput::rendered_blocks);
        let content = match rendered_process.as_deref() {
            Some(rendered) => ToolContentWireRef::RenderedProcess(rendered),
            None => ToolContentWireRef::Borrowed(&self.content),
        };
        let structured = match process {
            Some(output) => Some(StructuredWireRef::Process(output)),
            None => self.structured_for_wire().map(StructuredWireRef::Json),
        };

        ToolOutputWireRef {
            content,
            is_error: self.is_error,
            structured,
            duration: &self.duration,
            truncated: self.truncated,
            original_output_tokens: self.original_output_tokens,
            artifact: self.artifact.as_ref(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ToolOutput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self::from_wire(ToolOutputWire::deserialize(deserializer)?))
    }
}

impl ToolOutput {
    fn process_for_wire(&self) -> Result<Option<&ProcessOutput>, &'static str> {
        let mut processes = self.content.iter().filter_map(|block| match block {
            ToolContent::Process { output } => Some(output),
            ToolContent::Text { .. } | ToolContent::Json { .. } => None,
        });
        let Some(process) = processes.next() else {
            return Ok(None);
        };
        if self.content.len() != 1 || processes.next().is_some() || self.structured.is_some() {
            return Err(NONCANONICAL_PROCESS_WIRE_ERROR);
        }
        Ok(Some(process))
    }

    fn structured_for_wire(&self) -> Option<&Value> {
        self.structured.as_ref().or_else(|| {
            let mut json_blocks = self.content.iter().filter_map(|block| match block {
                ToolContent::Json { data } => Some(data),
                ToolContent::Text { .. } | ToolContent::Process { .. } => None,
            });
            let data = json_blocks.next()?;
            json_blocks.next().is_none().then_some(data)
        })
    }

    fn from_wire(wire: ToolOutputWire) -> Self {
        let mut content = wire
            .content
            .into_iter()
            .map(|block| match block {
                ToolContentWire::Text { text } => ToolContent::Text { text },
                ToolContentWire::Json { data } => ToolContent::Json { data },
                ToolContentWire::Process { output } => ToolContent::Process { output },
            })
            .collect::<Vec<_>>();
        let mut structured = wire.structured;

        let has_tagged_process = content
            .iter()
            .any(|block| matches!(block, ToolContent::Process { .. }));
        if structured.as_ref().is_some_and(|payload| {
            content
                .iter()
                .any(|block| matches!(block, ToolContent::Json { data } if data == payload))
        }) {
            // Parent JSON outputs copied the same payload into `content` and
            // `structured`. Keep the content block as the canonical carrier.
            structured = None;
        } else if !has_tagged_process
            && let Some(process) = structured.as_ref().and_then(legacy_process_output)
            && legacy_process_matches_wire(&content, wire.is_error, wire.truncated, &process)
        {
            // Parent process outputs copied stdout/stderr into rendered text
            // blocks and an exact three- or five-field structured object.
            content = vec![ToolContent::Process { output: process }];
            structured = None;
        }

        Self {
            content,
            is_error: wire.is_error,
            structured,
            duration: wire.duration,
            truncated: wire.truncated,
            original_output_tokens: wire.original_output_tokens,
            artifact: wire.artifact,
        }
    }

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

    /// Attaches a durable artifact reference for oversized output.
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

fn legacy_process_output(value: &Value) -> Option<ProcessOutput> {
    let object = value.as_object()?;
    const REQUIRED_KEYS: [&str; 3] = ["stdout", "stderr", "exit_code"];
    const TRUNCATION_KEYS: [&str; 2] = ["stdout_truncated", "stderr_truncated"];
    let exact_required = object.len() == REQUIRED_KEYS.len()
        && REQUIRED_KEYS.iter().all(|key| object.contains_key(*key));
    let exact_with_truncation = object.len() == REQUIRED_KEYS.len() + TRUNCATION_KEYS.len()
        && REQUIRED_KEYS.iter().all(|key| object.contains_key(*key))
        && TRUNCATION_KEYS.iter().all(|key| object.contains_key(*key));
    if !exact_required && !exact_with_truncation {
        return None;
    }
    ProcessOutput::deserialize(value).ok()
}

fn legacy_process_matches_wire(
    content: &[ToolContent],
    is_error: bool,
    truncated: bool,
    process: &ProcessOutput,
) -> bool {
    if is_error != (process.exit_code != 0)
        || truncated != (process.stdout_truncated || process.stderr_truncated)
    {
        return false;
    }
    let rendered = process.rendered_blocks();
    content.len() == rendered.len()
        && content.iter().zip(rendered).all(
            |(block, rendered)| matches!(block, ToolContent::Text { text } if text == &rendered),
        )
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
    /// Declared synchronous or asynchronous provider completion contract.
    pub async_mode: ToolAsyncMode,
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

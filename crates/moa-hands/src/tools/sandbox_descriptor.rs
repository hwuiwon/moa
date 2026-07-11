//! Descriptor-owned metadata for sandbox-backed hand tools.

use moa_core::{
    types::action_policy::ActionClass, types::action_policy::ActionPolicyEffect,
    types::action_policy::RiskLevel, types::tools::IdempotencyClass, types::tools::ToolDefinition,
    types::tools::ToolDiffStrategy, types::tools::ToolInputShape, types::tools::ToolPolicySpec,
};
use serde_json::{Value, json};

/// Stable executor capability implemented by one or more sandbox providers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SandboxToolCapability {
    /// Run a shell command.
    Bash,
    /// Search file contents.
    Grep,
    /// Return a structured file outline.
    FileOutline,
    /// Read a file.
    FileRead,
    /// Replace text in a file.
    StrReplace,
    /// Write a file.
    FileWrite,
    /// Search file names.
    FileSearch,
}

impl SandboxToolCapability {
    /// All sandbox capabilities in stable descriptor order.
    pub(crate) const ALL: [Self; 7] = [
        Self::Bash,
        Self::Grep,
        Self::FileOutline,
        Self::FileRead,
        Self::StrReplace,
        Self::FileWrite,
        Self::FileSearch,
    ];

    /// Returns the capability for a registered sandbox tool name.
    pub(crate) fn from_tool_name(tool_name: &str) -> Option<Self> {
        sandbox_tool_descriptor(tool_name).map(|descriptor| descriptor.capability)
    }
}

/// How sandbox tool inputs are normalized for policy matching.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SandboxNormalizationMetadata {
    /// Input shape used to extract the policy-facing value.
    pub(crate) input_shape: ToolInputShape,
    /// Action-pattern strategy used for admin-review rule suggestions.
    pub(crate) action_pattern: SandboxActionPattern,
}

/// Strategy for deriving an action-policy pattern from normalized input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SandboxActionPattern {
    /// Use the normalized input value as-is.
    NormalizedInput,
    /// Parse the shell input and suggest the first effective command.
    ShellFirstCommand,
}

/// Descriptor-owned action-review preview metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SandboxReviewPreviewMetadata {
    /// Command preview with an optional working-directory field.
    Command,
    /// Preview one string field from the input.
    SingleField {
        /// Input field containing the preview value.
        field: &'static str,
        /// Review label for the preview value.
        label: &'static str,
    },
    /// Preview a whole-file write.
    FileWrite {
        /// Input field containing the path.
        path_field: &'static str,
        /// Input field containing the file body.
        content_field: &'static str,
    },
    /// Preview a surgical string replacement.
    StrReplace {
        /// Input field containing the path.
        path_field: &'static str,
        /// Input field containing the old string.
        old_field: &'static str,
        /// Input field containing the new string.
        new_field: &'static str,
        /// Optional input field containing an insertion line.
        insert_after_line_field: &'static str,
    },
}

/// Metadata that defines one sandbox-backed tool.
#[derive(Debug)]
pub(crate) struct SandboxToolDescriptor {
    /// Stable registered tool name.
    pub(crate) name: &'static str,
    /// Prompt-facing default loadout position.
    pub(crate) default_loadout_position: usize,
    /// Human-readable prompt guidance.
    pub(crate) description: &'static str,
    /// JSON-schema factory for the tool input.
    pub(crate) schema: fn() -> Value,
    /// Static action-policy metadata.
    pub(crate) policy: ToolPolicySpec,
    /// Declared retry/idempotency semantics.
    pub(crate) idempotency_class: IdempotencyClass,
    /// Metadata used to normalize policy inputs.
    pub(crate) normalization: SandboxNormalizationMetadata,
    /// Metadata used to render admin-review previews.
    pub(crate) review_preview: SandboxReviewPreviewMetadata,
    /// Provider executor capability key.
    pub(crate) capability: SandboxToolCapability,
}

impl SandboxToolDescriptor {
    /// Builds the registry tool definition for this descriptor.
    pub(crate) fn definition(&self, max_output_tokens: u32) -> ToolDefinition {
        ToolDefinition {
            name: self.name.to_string(),
            description: self.description.to_string(),
            schema: (self.schema)(),
            policy: self.policy.clone(),
            idempotency_class: self.idempotency_class,
            max_output_tokens,
        }
    }
}

/// Returns every sandbox tool descriptor in stable descriptor order.
pub(crate) fn sandbox_tool_descriptors() -> &'static [SandboxToolDescriptor] {
    SANDBOX_TOOL_DESCRIPTORS
}

/// Returns sandbox descriptors in prompt-facing default loadout order.
pub(crate) fn default_sandbox_tool_descriptors() -> Vec<&'static SandboxToolDescriptor> {
    let mut descriptors = SANDBOX_TOOL_DESCRIPTORS.iter().collect::<Vec<_>>();
    descriptors.sort_by_key(|descriptor| descriptor.default_loadout_position);
    descriptors
}

/// Finds a sandbox tool descriptor by registered tool name.
pub(crate) fn sandbox_tool_descriptor(tool_name: &str) -> Option<&'static SandboxToolDescriptor> {
    SANDBOX_TOOL_DESCRIPTORS
        .iter()
        .find(|descriptor| descriptor.name == tool_name)
}

/// Resolves a supported capability for a provider-specific capability table.
pub(crate) fn supported_capability_for_tool(
    tool_name: &str,
    supported: &[SandboxToolCapability],
) -> Option<SandboxToolCapability> {
    SandboxToolCapability::from_tool_name(tool_name)
        .filter(|capability| supported.contains(capability))
}

/// Builds a provider-specific unsupported-tool error.
pub(crate) fn unsupported_tool(provider: &str, tool: &str) -> moa_core::error::MoaError {
    moa_core::error::MoaError::ToolError(format!("unsupported {provider} tool: {tool}"))
}

const fn execute_policy(input_shape: ToolInputShape) -> ToolPolicySpec {
    ToolPolicySpec {
        risk_level: RiskLevel::High,
        default_effect: ActionPolicyEffect::AdminReview,
        action_class: ActionClass::CommandExecution,
        input_shape,
        diff_strategy: ToolDiffStrategy::None,
    }
}

const fn read_policy(input_shape: ToolInputShape) -> ToolPolicySpec {
    ToolPolicySpec {
        risk_level: RiskLevel::Low,
        default_effect: ActionPolicyEffect::Allow,
        action_class: ActionClass::Read,
        input_shape,
        diff_strategy: ToolDiffStrategy::None,
    }
}

const fn write_policy(
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

const fn normalization(
    input_shape: ToolInputShape,
    action_pattern: SandboxActionPattern,
) -> SandboxNormalizationMetadata {
    SandboxNormalizationMetadata {
        input_shape,
        action_pattern,
    }
}

static SANDBOX_TOOL_DESCRIPTORS: &[SandboxToolDescriptor] = &[
    SandboxToolDescriptor {
        name: "bash",
        default_loadout_position: 6,
        description: "Purpose: run a non-interactive shell command inside the active workspace root. Use when: tests, builds, package managers, git inspection, or commands native file tools cannot express. Do not use: routine repository navigation, source reading, or text edits that file_search, grep, file_outline, file_read, str_replace, or file_write can handle. If blocked: keep commands targeted, preserve stderr/stdout, and stop after repeated failures instead of looping.",
        schema: bash_schema,
        policy: execute_policy(ToolInputShape::Command),
        idempotency_class: IdempotencyClass::NonIdempotent,
        normalization: normalization(
            ToolInputShape::Command,
            SandboxActionPattern::ShellFirstCommand,
        ),
        review_preview: SandboxReviewPreviewMetadata::Command,
        capability: SandboxToolCapability::Bash,
    },
    SandboxToolDescriptor {
        name: "grep",
        default_loadout_position: 1,
        description: "Purpose: search workspace file contents with regex or literal patterns. Use when: locating symbols, strings, errors, tests, or references before reading files. Do not use: broad exploratory filesystem walks or generated/vendor directories. If blocked: narrow path, enable literal matching for exact strings, or read a small matching range.",
        schema: grep_schema,
        policy: read_policy(ToolInputShape::Pattern),
        idempotency_class: IdempotencyClass::Idempotent,
        normalization: normalization(
            ToolInputShape::Pattern,
            SandboxActionPattern::NormalizedInput,
        ),
        review_preview: SandboxReviewPreviewMetadata::SingleField {
            field: "pattern",
            label: "Pattern",
        },
        capability: SandboxToolCapability::Grep,
    },
    SandboxToolDescriptor {
        name: "file_outline",
        default_loadout_position: 2,
        description: "Purpose: inspect a Python file's symbol outline without reading the full file. Use when: a large Python source file needs class, function, method, or line-number orientation. Do not use: non-Python files or exact content searches where grep is better. If blocked: fall back to a narrow file_read range after locating the nearest symbol.",
        schema: file_outline_schema,
        policy: read_policy(ToolInputShape::Path),
        idempotency_class: IdempotencyClass::Idempotent,
        normalization: normalization(ToolInputShape::Path, SandboxActionPattern::NormalizedInput),
        review_preview: SandboxReviewPreviewMetadata::SingleField {
            field: "path",
            label: "Path",
        },
        capability: SandboxToolCapability::FileOutline,
    },
    SandboxToolDescriptor {
        name: "file_read",
        default_loadout_position: 3,
        description: "Purpose: read UTF-8 text from a workspace file. Use when: you already know the relevant file or line range. Do not use: whole large files before searching or outlining. If blocked: use grep or file_outline first, then retry with a narrower start_line/end_line range.",
        schema: file_read_schema,
        policy: read_policy(ToolInputShape::Path),
        idempotency_class: IdempotencyClass::Idempotent,
        normalization: normalization(ToolInputShape::Path, SandboxActionPattern::NormalizedInput),
        review_preview: SandboxReviewPreviewMetadata::SingleField {
            field: "path",
            label: "Path",
        },
        capability: SandboxToolCapability::FileRead,
    },
    SandboxToolDescriptor {
        name: "str_replace",
        default_loadout_position: 4,
        description: "Purpose: replace one unique string match in an existing UTF-8 text file. Use when: editing an existing file with enough surrounding context to make old_str match exactly once. Do not use: new files, ambiguous matches, or line-number-only insertions. If blocked: read a narrower span and retry with more exact context.",
        schema: str_replace_schema,
        policy: write_policy(ToolInputShape::Path, ToolDiffStrategy::StrReplace),
        idempotency_class: IdempotencyClass::NonIdempotent,
        normalization: normalization(ToolInputShape::Path, SandboxActionPattern::NormalizedInput),
        review_preview: SandboxReviewPreviewMetadata::StrReplace {
            path_field: "path",
            old_field: "old_str",
            new_field: "new_str",
            insert_after_line_field: "insert_after_line",
        },
        capability: SandboxToolCapability::StrReplace,
    },
    SandboxToolDescriptor {
        name: "file_write",
        default_loadout_position: 5,
        description: "Purpose: create or deliberately overwrite a UTF-8 text file inside the active workspace root. Use when: adding a new file or replacing a whole generated/test fixture file intentionally. Do not use: small edits to existing source files where str_replace is safer. If blocked: verify the relative path and avoid `..` or paths outside the workspace.",
        schema: file_write_schema,
        policy: write_policy(ToolInputShape::Path, ToolDiffStrategy::FileWrite),
        idempotency_class: IdempotencyClass::NonIdempotent,
        normalization: normalization(ToolInputShape::Path, SandboxActionPattern::NormalizedInput),
        review_preview: SandboxReviewPreviewMetadata::FileWrite {
            path_field: "path",
            content_field: "content",
        },
        capability: SandboxToolCapability::FileWrite,
    },
    SandboxToolDescriptor {
        name: "file_search",
        default_loadout_position: 0,
        description: "Purpose: find files inside the active workspace root with a glob pattern. Use when: locating paths before reading or editing. Do not use: content search, shell globbing, or generated/vendor directory exploration. If blocked: tighten the glob or switch to grep when the identifier is content rather than a path.",
        schema: file_search_schema,
        policy: read_policy(ToolInputShape::Pattern),
        idempotency_class: IdempotencyClass::Idempotent,
        normalization: normalization(
            ToolInputShape::Pattern,
            SandboxActionPattern::NormalizedInput,
        ),
        review_preview: SandboxReviewPreviewMetadata::SingleField {
            field: "pattern",
            label: "Pattern",
        },
        capability: SandboxToolCapability::FileSearch,
    },
];

fn bash_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "cmd": { "type": "string", "description": "Shell command to execute." },
            "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 300, "description": "Optional timeout override in seconds." }
        },
        "required": ["cmd"],
        "additionalProperties": false
    })
}

fn grep_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": { "type": "string", "description": "Regex pattern to search for. Use literal for exact string matching." },
            "path": { "type": "string", "description": "Optional subdirectory or file to search within. Defaults to the workspace root." },
            "context_lines": { "type": "integer", "minimum": 0, "maximum": 5, "description": "Optional number of surrounding lines to include for each match. Default: 0." },
            "literal": { "type": "boolean", "description": "Treat pattern as a literal string instead of a regex. Default: false." }
        },
        "required": ["pattern"],
        "additionalProperties": false
    })
}

fn file_outline_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Relative path within the workspace root. Currently supports Python files." },
            "symbol": { "type": "string", "description": "Optional class, function, or method name to focus on." }
        },
        "required": ["path"],
        "additionalProperties": false
    })
}

fn file_read_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Relative path within the workspace root. Bash `cd` state does not carry over." },
            "start_line": { "type": "integer", "minimum": 1, "description": "Optional 1-based first line to read, inclusive." },
            "end_line": { "type": "integer", "minimum": 1, "description": "Optional 1-based last line to read, inclusive. Ranges are clamped and truncated to 200 lines." }
        },
        "required": ["path"],
        "additionalProperties": false
    })
}

fn str_replace_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Relative path within the workspace root. Bash `cd` state does not carry over." },
            "old_str": { "type": "string", "description": "Exact existing string to replace. Must be non-empty and must match exactly once." },
            "new_str": { "type": "string", "description": "Replacement string. Empty deletes the matched region." }
        },
        "required": ["path", "old_str", "new_str"],
        "additionalProperties": false
    })
}

fn file_write_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Relative path within the workspace root. Bash `cd` state does not carry over." },
            "content": { "type": "string", "description": "Full file contents to write." }
        },
        "required": ["path", "content"],
        "additionalProperties": false
    })
}

fn file_search_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": { "type": "string", "description": "Glob pattern such as **/*.rs, evaluated from the workspace root." }
        },
        "required": ["pattern"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{
        SandboxToolCapability, default_sandbox_tool_descriptors, sandbox_tool_descriptors,
    };

    #[test]
    fn sandbox_descriptor_names_are_unique_and_parseable() {
        // Pins: sandbox tool names are owned by descriptors and map back to capabilities.
        let mut names = HashSet::new();
        for descriptor in sandbox_tool_descriptors() {
            assert!(
                names.insert(descriptor.name),
                "duplicate sandbox descriptor name: {}",
                descriptor.name
            );
            assert_eq!(
                SandboxToolCapability::from_tool_name(descriptor.name),
                Some(descriptor.capability)
            );
        }
        assert_eq!(SandboxToolCapability::from_tool_name("unknown"), None);
    }

    #[test]
    fn default_loadout_keeps_prompt_order() {
        // Pins: descriptor-owned default loadout preserves the existing prompt order.
        let loadout = default_sandbox_tool_descriptors()
            .into_iter()
            .map(|descriptor| descriptor.name)
            .collect::<Vec<_>>();
        assert_eq!(
            loadout,
            [
                "file_search",
                "grep",
                "file_outline",
                "file_read",
                "str_replace",
                "file_write",
                "bash",
            ]
        );
    }

    #[test]
    fn capabilities_have_descriptors() {
        // Pins: provider capability tables cannot name a capability with no descriptor.
        for capability in SandboxToolCapability::ALL {
            assert!(
                sandbox_tool_descriptors()
                    .iter()
                    .any(|descriptor| descriptor.capability == capability),
                "missing descriptor for capability {capability:?}"
            );
        }
    }
}

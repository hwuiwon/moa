//! Budgeting, context, compaction, and task-resolution configuration.

use serde::{Deserialize, Serialize};

/// Workspace-level cost budget settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetConfig {
    /// Maximum daily spend per workspace in cents. `0` disables budget enforcement.
    pub daily_workspace_cents: u32,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            daily_workspace_cents: 2_000,
        }
    }
}

/// Per-session turn and loop guardrails.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionLimitsConfig {
    /// Maximum completed turns per session before pausing. `0` disables the limit.
    pub max_turns: u32,
    /// Number of identical consecutive turn fingerprints that triggers a loop pause. `0` disables detection.
    pub loop_detection_threshold: u32,
}

impl Default for SessionLimitsConfig {
    fn default() -> Self {
        Self {
            max_turns: 50,
            loop_detection_threshold: 3,
        }
    }
}

/// Tool-output truncation settings for storage and history replay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolOutputConfig {
    /// Maximum characters for replayed tool output.
    pub max_replay_chars: usize,
    /// Maximum preserved lines for bash output before head+tail truncation.
    pub max_bash_lines: usize,
    /// Fraction of the truncation budget allocated to the head of the output.
    pub head_ratio: f64,
}

impl Default for ToolOutputConfig {
    fn default() -> Self {
        Self {
            max_replay_chars: 20_000,
            max_bash_lines: 200,
            head_ratio: 0.4,
        }
    }
}

/// Per-tool router-level output budgets enforced before event persistence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolBudgetConfig {
    /// Approximate token budget for `file_read`.
    pub file_read: u32,
    /// Approximate token budget for successful `bash` stdout.
    pub bash_stdout: u32,
    /// Approximate token budget for successful `bash` stderr.
    pub bash_stderr: u32,
    /// Approximate token budget for `grep`.
    pub grep: u32,
    /// Approximate token budget for `file_search`.
    pub file_search: u32,
    /// Approximate token budget for `memory_search`.
    pub memory_search: u32,
    /// Approximate token budget for `file_outline`.
    pub file_outline: u32,
    /// Approximate token budget for tools without a dedicated override, including MCP tools.
    pub default: u32,
}

impl ToolBudgetConfig {
    /// Returns the configured total output budget for one successful tool invocation.
    pub fn for_tool(&self, tool_name: &str) -> u32 {
        match tool_name {
            "bash" => self.bash_stdout,
            "file_read" => self.file_read,
            "grep" => self.grep,
            "file_search" => self.file_search,
            "memory_search" => self.memory_search,
            "file_outline" => self.file_outline,
            _ => self.default,
        }
    }
}

impl Default for ToolBudgetConfig {
    fn default() -> Self {
        Self {
            file_read: 8_000,
            bash_stdout: 4_000,
            bash_stderr: 2_000,
            grep: 4_000,
            file_search: 4_000,
            memory_search: 3_000,
            file_outline: 2_000,
            default: 8_000,
        }
    }
}

/// Incremental context snapshot configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextSnapshotConfig {
    /// Whether compiled context snapshots are enabled.
    pub enabled: bool,
    /// Warn when a serialized snapshot exceeds this size.
    pub max_size_bytes: usize,
}

impl Default for ContextSnapshotConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_size_bytes: 5_000_000,
        }
    }
}

/// Stage-4 skill-manifest budgeting controls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SkillBudgetConfig {
    /// Maximum characters for the entire skill manifest.
    ///
    /// `None` uses `max(context_window * 0.01, 8000)` at runtime.
    pub max_manifest_chars: Option<usize>,
    /// Maximum characters for one individual skill entry before truncation.
    pub max_per_skill_chars: usize,
    /// Whether manifest entries should include estimated token counts.
    pub show_token_estimates: bool,
}

impl Default for SkillBudgetConfig {
    fn default() -> Self {
        Self {
            max_manifest_chars: None,
            max_per_skill_chars: 1_536,
            show_token_estimates: true,
        }
    }
}

/// Query-rewriting controls for the context pipeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QueryRewriteConfig {
    /// Whether query rewriting is enabled.
    pub enabled: bool,
    /// Model to use for rewriting. Defaults to the selected auxiliary provider.
    pub model: Option<String>,
    /// Hard timeout for the rewriter LLM call.
    pub timeout_ms: u64,
    /// Minimum token count in a single-turn query to trigger rewriting.
    pub min_query_tokens: usize,
    /// Whether to skip rewriting on single-turn conversations below the token threshold.
    pub skip_single_turn: bool,
    /// Circuit-breaker error-rate threshold that disables rewriting.
    pub circuit_breaker_threshold: f64,
    /// Circuit-breaker sliding window length in seconds.
    pub circuit_breaker_window_secs: u64,
    /// Circuit-breaker cooldown length in seconds after tripping.
    pub circuit_breaker_cooldown_secs: u64,
}

impl Default for QueryRewriteConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model: None,
            timeout_ms: 5_000,
            min_query_tokens: 15,
            skip_single_turn: true,
            circuit_breaker_threshold: 0.05,
            circuit_breaker_window_secs: 60,
            circuit_breaker_cooldown_secs: 60,
        }
    }
}

/// Automated task-segment resolution scoring controls.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResolutionConfig {
    /// Whether automated resolution scoring is enabled.
    pub enabled: bool,
    /// Signal weights used by the composite scorer.
    pub weights: ResolutionWeights,
    /// Whether ambiguous agent self-assessment should use an LLM fallback.
    pub use_llm_self_assessment: bool,
    /// Timeout for optional LLM self-assessment.
    pub self_assessment_timeout_ms: u64,
    /// Similarity threshold above which a later user message is treated as a rephrase.
    pub rephrase_similarity_threshold: f64,
    /// Minimum historical sample count before structural baselines are used.
    pub structural_min_samples: usize,
    /// Idle timeout used for final continuation scoring.
    pub idle_timeout_minutes: u64,
}

impl Default for ResolutionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            weights: ResolutionWeights::default(),
            use_llm_self_assessment: false,
            self_assessment_timeout_ms: 300,
            rephrase_similarity_threshold: 0.85,
            structural_min_samples: 20,
            idle_timeout_minutes: 30,
        }
    }
}

/// Composite scorer weights for individual resolution signals.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResolutionWeights {
    /// Weight assigned to tool outcome analysis.
    pub tool: f64,
    /// Weight assigned to verification command detection.
    pub verification: f64,
    /// Weight assigned to user continuation behavior.
    pub continuation: f64,
    /// Weight assigned to agent final-response self-assessment.
    pub self_assessment: f64,
    /// Weight assigned to structural anomaly detection.
    pub structural: f64,
}

impl Default for ResolutionWeights {
    fn default() -> Self {
        Self {
            tool: 0.20,
            verification: 0.30,
            continuation: 0.25,
            self_assessment: 0.15,
            structural: 0.10,
        }
    }
}

/// Session-history compaction configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionConfig {
    /// Whether reversible history compaction is enabled.
    pub enabled: bool,
    /// Emit a checkpoint after this many unsummarized events.
    pub event_threshold: usize,
    /// Emit a checkpoint after unsummarized history reaches this fraction of the token budget.
    pub token_ratio_threshold: f64,
    /// Number of most recent user turns to keep verbatim in context.
    pub recent_turns_verbatim: usize,
    /// Whether old error events must stay verbatim in the compiled view.
    pub preserve_errors: bool,
    /// Trigger cache-aware trimming when older history exceeds this many blocks.
    pub tier2_trigger_blocks_past_bp4: usize,
    /// Trigger summarization when the turn approaches this fraction of the model context window.
    pub tier3_trigger_fraction: f64,
    /// Hard ceiling for input tokens per turn after compaction.
    pub max_input_tokens_per_turn: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            event_threshold: 100,
            token_ratio_threshold: 0.7,
            recent_turns_verbatim: 5,
            preserve_errors: true,
            tier2_trigger_blocks_past_bp4: 14,
            tier3_trigger_fraction: 0.9,
            max_input_tokens_per_turn: 160_000,
        }
    }
}

//! Budgeting, context, compaction, and task-segment assessment configuration.

use serde::{Deserialize, Serialize};

/// Tenant-level cost budget settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetConfig {
    /// Maximum daily spend per tenant in cents. `0` disables budget enforcement.
    pub daily_tenant_cents: u32,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            daily_tenant_cents: 2_000,
        }
    }
}

/// Per-session turn and loop guardrails.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionLimitsConfig {
    /// Maximum completed turns per session before pausing. `0` disables the limit.
    pub max_turns: u32,
    /// Maximum model loop iterations for requests classified as simple.
    pub simple_max_turns: u32,
    /// Maximum model loop iterations for requests classified as standard.
    pub standard_max_turns: u32,
    /// Maximum tool calls allowed within one turn. `0` disables tool calls.
    pub max_tool_calls: u32,
    /// Number of identical consecutive turn fingerprints that triggers a loop pause. `0` disables detection.
    pub loop_detection_threshold: u32,
    /// Delay before the first durable progress update is eligible, in milliseconds.
    pub progress_first_delay_ms: u64,
    /// Minimum interval between durable progress updates, in milliseconds.
    pub progress_interval_ms: u64,
    /// Whether default-on natural-language progress narration is enabled.
    pub progress_narration_enabled: bool,
    /// Optional model id override for progress narration. `None` selects the
    /// model catalog's cheapest chat-capable model by combined token price.
    pub progress_narration_model: Option<String>,
    /// Minimum interval between progress narrations, in milliseconds. Consumed by
    /// the per-session narration tick that dispatches the narration job.
    pub progress_narration_interval_ms: u64,
    /// Maximum number of narrations per rolling window before the narrator backs
    /// off. Consumed by the per-session narration tick.
    pub progress_narration_max_per_window: u32,
    /// Maximum output tokens for one progress-narration completion.
    pub progress_narration_max_tokens: u32,
    /// Grace window before a terminal sub-agent self-cleans (removes itself from the
    /// parent fan-out and clears its VO state) after reporting its result. A follow-up
    /// arriving within this window revives the child instead of letting it clean up.
    /// `0` disables self-cleanup scheduling.
    pub sub_agent_cleanup_grace_ms: u64,
}

impl Default for SessionLimitsConfig {
    fn default() -> Self {
        Self {
            max_turns: 50,
            simple_max_turns: 1,
            standard_max_turns: 6,
            max_tool_calls: 30,
            loop_detection_threshold: 3,
            progress_first_delay_ms: 8_000,
            progress_interval_ms: 8_000,
            progress_narration_enabled: true,
            progress_narration_model: None,
            progress_narration_interval_ms: 20_000,
            progress_narration_max_per_window: 30,
            progress_narration_max_tokens: 120,
            sub_agent_cleanup_grace_ms: 60_000,
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

/// Automated task-segment assessment controls.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResolutionConfig {
    /// Whether automated segment assessment is enabled.
    pub enabled: bool,
    /// Signal weights used by the composite assessor.
    pub weights: ResolutionWeights,
    /// Similarity threshold above which a later user message is treated as a rephrase.
    pub rephrase_similarity_threshold: f64,
    /// Minimum historical sample count before structural baselines are used.
    pub structural_min_samples: usize,
    /// Idle timeout used for final continuation assessment.
    pub idle_timeout_minutes: u64,
}

impl Default for ResolutionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            weights: ResolutionWeights::default(),
            rephrase_similarity_threshold: 0.85,
            structural_min_samples: 20,
            idle_timeout_minutes: 30,
        }
    }
}

/// Composite assessor weights for individual segment signals.
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

impl super::MoaEnvOverlay {
    /// Applies budgeting, compaction, session-limit, tool, rewrite, and resolution overrides.
    pub(in crate::config) fn apply_context_overlay(&self, config: &mut super::MoaConfig) {
        use super::env_overlay::{set_copy_if_some, set_option_if_some};

        set_copy_if_some(
            &mut config.budgets.daily_tenant_cents,
            self.budgets_daily_tenant_cents,
        );
        set_copy_if_some(
            &mut config.session_limits.max_turns,
            self.session_limits_max_turns,
        );
        set_copy_if_some(
            &mut config.session_limits.simple_max_turns,
            self.session_limits_simple_max_turns,
        );
        set_copy_if_some(
            &mut config.session_limits.standard_max_turns,
            self.session_limits_standard_max_turns,
        );
        set_copy_if_some(
            &mut config.session_limits.max_tool_calls,
            self.session_limits_max_tool_calls,
        );
        set_copy_if_some(
            &mut config.session_limits.loop_detection_threshold,
            self.session_limits_loop_detection_threshold,
        );
        set_copy_if_some(
            &mut config.session_limits.progress_first_delay_ms,
            self.session_limits_progress_first_delay_ms,
        );
        set_copy_if_some(
            &mut config.session_limits.progress_interval_ms,
            self.session_limits_progress_interval_ms,
        );
        set_copy_if_some(
            &mut config.session_limits.progress_narration_enabled,
            self.session_limits_progress_narration_enabled,
        );
        set_option_if_some(
            &mut config.session_limits.progress_narration_model,
            &self.session_limits_progress_narration_model,
        );
        set_copy_if_some(
            &mut config.session_limits.progress_narration_interval_ms,
            self.session_limits_progress_narration_interval_ms,
        );
        set_copy_if_some(
            &mut config.session_limits.progress_narration_max_per_window,
            self.session_limits_progress_narration_max_per_window,
        );
        set_copy_if_some(
            &mut config.session_limits.progress_narration_max_tokens,
            self.session_limits_progress_narration_max_tokens,
        );
        set_copy_if_some(
            &mut config.session_limits.sub_agent_cleanup_grace_ms,
            self.session_limits_sub_agent_cleanup_grace_ms,
        );
        self.apply_tooling(config);
        self.apply_query_rewrite(config);
        self.apply_resolution(config);
        set_copy_if_some(
            &mut config.context_snapshot.enabled,
            self.context_snapshot_enabled,
        );
        set_copy_if_some(
            &mut config.context_snapshot.max_size_bytes,
            self.context_snapshot_max_size_bytes,
        );
    }

    /// Applies session-history compaction environment overrides.
    pub(in crate::config) fn apply_compaction_overlay(&self, config: &mut super::MoaConfig) {
        use super::env_overlay::set_copy_if_some;

        set_copy_if_some(&mut config.compaction.enabled, self.compaction_enabled);
        set_copy_if_some(
            &mut config.compaction.event_threshold,
            self.compaction_event_threshold,
        );
        set_copy_if_some(
            &mut config.compaction.token_ratio_threshold,
            self.compaction_token_ratio_threshold,
        );
        set_copy_if_some(
            &mut config.compaction.recent_turns_verbatim,
            self.compaction_recent_turns_verbatim,
        );
        set_copy_if_some(
            &mut config.compaction.preserve_errors,
            self.compaction_preserve_errors,
        );
        set_copy_if_some(
            &mut config.compaction.tier2_trigger_blocks_past_bp4,
            self.compaction_tier2_trigger_blocks_past_bp4,
        );
        set_copy_if_some(
            &mut config.compaction.tier3_trigger_fraction,
            self.compaction_tier3_trigger_fraction,
        );
        set_copy_if_some(
            &mut config.compaction.max_input_tokens_per_turn,
            self.compaction_max_input_tokens_per_turn,
        );
    }

    fn apply_tooling(&self, config: &mut super::MoaConfig) {
        use super::env_overlay::set_copy_if_some;

        set_copy_if_some(
            &mut config.tool_output.max_replay_chars,
            self.tool_output_max_replay_chars,
        );
        set_copy_if_some(
            &mut config.tool_output.max_bash_lines,
            self.tool_output_max_bash_lines,
        );
        set_copy_if_some(
            &mut config.tool_output.head_ratio,
            self.tool_output_head_ratio,
        );
        set_copy_if_some(
            &mut config.tool_budgets.file_read,
            self.tool_budgets_file_read,
        );
        set_copy_if_some(
            &mut config.tool_budgets.bash_stdout,
            self.tool_budgets_bash_stdout,
        );
        set_copy_if_some(
            &mut config.tool_budgets.bash_stderr,
            self.tool_budgets_bash_stderr,
        );
        set_copy_if_some(&mut config.tool_budgets.grep, self.tool_budgets_grep);
        set_copy_if_some(
            &mut config.tool_budgets.file_search,
            self.tool_budgets_file_search,
        );
        set_copy_if_some(
            &mut config.tool_budgets.memory_search,
            self.tool_budgets_memory_search,
        );
        set_copy_if_some(
            &mut config.tool_budgets.file_outline,
            self.tool_budgets_file_outline,
        );
        set_copy_if_some(&mut config.tool_budgets.default, self.tool_budgets_default);
        if let Some(max_manifest_chars) = self.skill_budget_max_manifest_chars {
            config.skill_budget.max_manifest_chars = Some(max_manifest_chars);
        }
        set_copy_if_some(
            &mut config.skill_budget.max_per_skill_chars,
            self.skill_budget_max_per_skill_chars,
        );
        set_copy_if_some(
            &mut config.skill_budget.show_token_estimates,
            self.skill_budget_show_token_estimates,
        );
    }

    fn apply_query_rewrite(&self, config: &mut super::MoaConfig) {
        use super::env_overlay::{set_copy_if_some, set_option_if_some};

        set_copy_if_some(
            &mut config.query_rewrite.enabled,
            self.query_rewrite_enabled,
        );
        set_option_if_some(&mut config.query_rewrite.model, &self.query_rewrite_model);
        set_copy_if_some(
            &mut config.query_rewrite.timeout_ms,
            self.query_rewrite_timeout_ms,
        );
        set_copy_if_some(
            &mut config.query_rewrite.min_query_tokens,
            self.query_rewrite_min_query_tokens,
        );
        set_copy_if_some(
            &mut config.query_rewrite.skip_single_turn,
            self.query_rewrite_skip_single_turn,
        );
        set_copy_if_some(
            &mut config.query_rewrite.circuit_breaker_threshold,
            self.query_rewrite_circuit_breaker_threshold,
        );
        set_copy_if_some(
            &mut config.query_rewrite.circuit_breaker_window_secs,
            self.query_rewrite_circuit_breaker_window_secs,
        );
        set_copy_if_some(
            &mut config.query_rewrite.circuit_breaker_cooldown_secs,
            self.query_rewrite_circuit_breaker_cooldown_secs,
        );
    }

    fn apply_resolution(&self, config: &mut super::MoaConfig) {
        use super::env_overlay::set_copy_if_some;

        set_copy_if_some(&mut config.resolution.enabled, self.resolution_enabled);
        set_copy_if_some(
            &mut config.resolution.weights.tool,
            self.resolution_weights_tool,
        );
        set_copy_if_some(
            &mut config.resolution.weights.verification,
            self.resolution_weights_verification,
        );
        set_copy_if_some(
            &mut config.resolution.weights.continuation,
            self.resolution_weights_continuation,
        );
        set_copy_if_some(
            &mut config.resolution.weights.self_assessment,
            self.resolution_weights_self_assessment,
        );
        set_copy_if_some(
            &mut config.resolution.weights.structural,
            self.resolution_weights_structural,
        );
        set_copy_if_some(
            &mut config.resolution.rephrase_similarity_threshold,
            self.resolution_rephrase_similarity_threshold,
        );
        set_copy_if_some(
            &mut config.resolution.structural_min_samples,
            self.resolution_structural_min_samples,
        );
        set_copy_if_some(
            &mut config.resolution.idle_timeout_minutes,
            self.resolution_idle_timeout_minutes,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::SessionLimitsConfig;
    use crate::config::{MoaConfig, MoaEnvOverlay};

    #[test]
    fn progress_narration_defaults_are_on_with_cheapest_model() {
        // Pins: narration ships default-on, catalog-cheapest model, with bounded cadence/cost.
        let limits = SessionLimitsConfig::default();
        assert!(limits.progress_narration_enabled);
        assert_eq!(limits.progress_narration_model, None);
        assert_eq!(limits.progress_narration_interval_ms, 20_000);
        assert_eq!(limits.progress_narration_max_per_window, 30);
        assert_eq!(limits.progress_narration_max_tokens, 120);
    }

    #[test]
    fn progress_narration_env_overlay_overrides_defaults() {
        // Pins: each MOA_SESSION_LIMITS_PROGRESS_NARRATION_* flat env var maps to its field.
        let overlay = MoaEnvOverlay {
            session_limits_progress_narration_enabled: Some(false),
            session_limits_progress_narration_model: Some("gpt-5-nano".to_string()),
            session_limits_progress_narration_interval_ms: Some(45_000),
            session_limits_progress_narration_max_per_window: Some(7),
            session_limits_progress_narration_max_tokens: Some(64),
            ..MoaEnvOverlay::default()
        };

        let mut config = MoaConfig::default();
        overlay
            .apply_to(&mut config)
            .expect("narration overlay should apply");

        let limits = &config.session_limits;
        assert!(!limits.progress_narration_enabled);
        assert_eq!(
            limits.progress_narration_model.as_deref(),
            Some("gpt-5-nano")
        );
        assert_eq!(limits.progress_narration_interval_ms, 45_000);
        assert_eq!(limits.progress_narration_max_per_window, 7);
        assert_eq!(limits.progress_narration_max_tokens, 64);
    }
}

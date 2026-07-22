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
    /// Fleet-wide maximum number of concurrently active coordinator turns.
    pub turn_admission_fleet_limit: u32,
    /// Per-tenant maximum number of concurrently active coordinator turns.
    pub turn_admission_tenant_limit: u32,
    /// TTL for one shared turn-admission lease, in milliseconds.
    pub turn_admission_lease_ttl_ms: u64,
    /// Retry delay returned to callers rejected by turn admission, in milliseconds.
    pub turn_admission_retry_after_ms: u64,
    /// Maximum messages retained behind one already-active session turn.
    pub max_pending_messages: u32,
    /// Maximum completed turns per session before pausing. `0` disables the limit.
    pub max_turns: u32,
    /// Maximum model loop iterations for requests classified as simple.
    pub simple_max_turns: u32,
    /// Maximum model loop iterations for requests classified as standard.
    pub standard_max_turns: u32,
    /// Maximum model loop iterations once a standard turn has delegated to at
    /// least one worker. Spawning workers, waiting for them, and synthesizing
    /// their results legitimately needs more turns than a non-delegating turn, so
    /// after the first successful worker spawn this cap replaces the base cap for
    /// the remainder of that turn. Escalation is one-way and never lowers the base
    /// cap. Bounded by `max_turns`.
    pub max_model_turns_delegation: u32,
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
    /// Grace window before a terminal worker self-cleans (removes itself from the
    /// parent fan-out and clears its VO state) after reporting its result. A follow-up
    /// arriving within this window revives the child instead of letting it clean up.
    /// `0` disables self-cleanup scheduling.
    pub worker_cleanup_grace_ms: u64,
    /// Maximum guarded coordinator auto-resumes dispatched per rolling window before the
    /// resume path backs off. `0` disables guarded parent resume entirely.
    pub worker_resume_max_per_window: u32,
    /// Rolling-window length, in milliseconds, for the guarded parent-resume budget.
    pub worker_resume_window_ms: u64,
    /// Maximum time a child `request_input` round-trip blocks on its awakeable before
    /// returning a "no input received" result so the child can proceed or abort. Kept
    /// large because a human answer (audience = user) may take minutes.
    pub worker_input_timeout_ms: u64,
    /// Target cadence, in milliseconds, at which an active child refreshes its
    /// telemetry-plane heartbeat while running. Sizes the heartbeat the watchdog
    /// observes; consumers treat `0` as the built-in default cadence.
    pub worker_heartbeat_interval_ms: u64,
    /// Age, in milliseconds, beyond which an active child's last heartbeat is treated
    /// as stale by the per-child liveness watchdog. The watchdog schedules its delayed
    /// self-check at this interval and raises a non-fatal `HeartbeatStale` signal when
    /// the threshold is exceeded. `0` disables the watchdog.
    pub worker_heartbeat_stale_ms: u64,
}

impl Default for SessionLimitsConfig {
    fn default() -> Self {
        Self {
            turn_admission_fleet_limit: 1_000,
            turn_admission_tenant_limit: 250,
            turn_admission_lease_ttl_ms: 600_000,
            turn_admission_retry_after_ms: 1_000,
            max_pending_messages: 8,
            max_turns: 50,
            simple_max_turns: 1,
            standard_max_turns: 6,
            max_model_turns_delegation: 12,
            max_tool_calls: 30,
            loop_detection_threshold: 3,
            progress_first_delay_ms: 8_000,
            progress_interval_ms: 8_000,
            progress_narration_enabled: true,
            progress_narration_model: None,
            progress_narration_interval_ms: 20_000,
            progress_narration_max_per_window: 30,
            progress_narration_max_tokens: 120,
            worker_cleanup_grace_ms: 60_000,
            worker_resume_max_per_window: 6,
            worker_resume_window_ms: 600_000,
            worker_input_timeout_ms: 1_800_000,
            worker_heartbeat_interval_ms: 15_000,
            worker_heartbeat_stale_ms: 60_000,
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
    ///
    /// Deliberately below `file_read`: MCP and other unclassified tools return
    /// the most verbose, least-trusted output, and oversized results stay
    /// recoverable through the claim-check artifact path.
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
            default: 4_000,
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
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            event_threshold: 100,
            token_ratio_threshold: 0.7,
            recent_turns_verbatim: 5,
            preserve_errors: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SessionLimitsConfig;
    use crate::{EnvOverlay, MoaConfig};

    #[test]
    fn turn_admission_defaults_are_fleet_bounded_and_queue_bounded() {
        // Pins: coordinator turns use a shared finite fleet/tenant budget and
        // messages behind an active session cannot grow without bound.
        let limits = SessionLimitsConfig::default();
        assert_eq!(limits.turn_admission_fleet_limit, 1_000);
        assert_eq!(limits.turn_admission_tenant_limit, 250);
        assert_eq!(limits.turn_admission_lease_ttl_ms, 600_000);
        assert_eq!(limits.turn_admission_retry_after_ms, 1_000);
        assert_eq!(limits.max_pending_messages, 8);
    }

    #[test]
    fn turn_admission_env_overlay_overrides_defaults() {
        // Pins: Kubernetes can tune every admission and pending-queue control
        // through the flat MOA_SESSION_LIMITS_* environment surface.
        let overlay = EnvOverlay {
            session_limits_turn_admission_fleet_limit: Some(400),
            session_limits_turn_admission_tenant_limit: Some(80),
            session_limits_turn_admission_lease_ttl_ms: Some(120_000),
            session_limits_turn_admission_retry_after_ms: Some(2_500),
            session_limits_max_pending_messages: Some(3),
            ..EnvOverlay::default()
        };
        let mut config = MoaConfig::default();
        overlay
            .apply_to(&mut config)
            .expect("turn admission overlay should apply");
        let limits = config.session_limits;
        assert_eq!(limits.turn_admission_fleet_limit, 400);
        assert_eq!(limits.turn_admission_tenant_limit, 80);
        assert_eq!(limits.turn_admission_lease_ttl_ms, 120_000);
        assert_eq!(limits.turn_admission_retry_after_ms, 2_500);
        assert_eq!(limits.max_pending_messages, 3);
    }

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
    fn worker_resume_budget_defaults_are_bounded() {
        // Pins: guarded parent resume ships with a finite per-window budget and window.
        let limits = SessionLimitsConfig::default();
        assert_eq!(limits.worker_resume_max_per_window, 6);
        assert_eq!(limits.worker_resume_window_ms, 600_000);
        // The needs_input round-trip ships with a large (but finite) default so a human
        // answer has time to arrive without blocking a child turn forever.
        assert_eq!(limits.worker_input_timeout_ms, 1_800_000);
    }

    #[test]
    fn worker_resume_budget_env_overlay_overrides_defaults() {
        // Pins: each MOA_SESSION_LIMITS_WORKER_RESUME_* flat env var maps to its field.
        let overlay = EnvOverlay {
            session_limits_worker_resume_max_per_window: Some(3),
            session_limits_worker_resume_window_ms: Some(120_000),
            session_limits_worker_input_timeout_ms: Some(90_000),
            ..EnvOverlay::default()
        };

        let mut config = MoaConfig::default();
        overlay
            .apply_to(&mut config)
            .expect("resume budget overlay should apply");

        let limits = &config.session_limits;
        assert_eq!(limits.worker_resume_max_per_window, 3);
        assert_eq!(limits.worker_resume_window_ms, 120_000);
        assert_eq!(limits.worker_input_timeout_ms, 90_000);
    }

    #[test]
    fn worker_heartbeat_defaults_and_env_overlay_override() {
        // Pins: the watchdog cadence/threshold ship with bounded defaults and each flat
        // MOA_SESSION_LIMITS_WORKER_HEARTBEAT_* env var maps to its field.
        let limits = SessionLimitsConfig::default();
        assert_eq!(limits.worker_heartbeat_interval_ms, 15_000);
        assert_eq!(limits.worker_heartbeat_stale_ms, 60_000);

        let overlay = EnvOverlay {
            session_limits_worker_heartbeat_interval_ms: Some(5_000),
            session_limits_worker_heartbeat_stale_ms: Some(30_000),
            ..EnvOverlay::default()
        };
        let mut config = MoaConfig::default();
        overlay
            .apply_to(&mut config)
            .expect("heartbeat overlay should apply");
        assert_eq!(config.session_limits.worker_heartbeat_interval_ms, 5_000);
        assert_eq!(config.session_limits.worker_heartbeat_stale_ms, 30_000);
    }

    #[test]
    fn progress_narration_env_overlay_overrides_defaults() {
        // Pins: each MOA_SESSION_LIMITS_PROGRESS_NARRATION_* flat env var maps to its field.
        let overlay = EnvOverlay {
            session_limits_progress_narration_enabled: Some(false),
            session_limits_progress_narration_model: Some("gpt-5-nano".to_string()),
            session_limits_progress_narration_interval_ms: Some(45_000),
            session_limits_progress_narration_max_per_window: Some(7),
            session_limits_progress_narration_max_tokens: Some(64),
            ..EnvOverlay::default()
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

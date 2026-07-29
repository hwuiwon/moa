//! Score card schema and analytics-score flattening helpers.

use chrono::{DateTime, Utc};
use moa_core::{
    types::identifiers::SessionId, types::identifiers::StoragePartitionId,
    types::identifiers::UserId,
};
use moa_eval_core::ConversationCost;
use moa_lineage_core::{ScoreRecord, ScoreSource, ScoreTarget, ScoreValue as LineageScoreValue};
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};
use uuid::Uuid;

/// Long-conversation score card consumed by regression dashboards.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreCard {
    /// Stable scenario name.
    pub scenario: String,
    /// Unique run identifier shared by every emitted score row.
    pub run_id: Uuid,
    /// Score-card creation timestamp.
    pub timestamp: DateTime<Utc>,
    /// Provider mode or provider name used for the run.
    pub provider: String,
    /// Functional correctness scores.
    pub functional: FunctionalScores,
    /// Latency scores in milliseconds.
    pub latency_ms: LatencyScores,
    /// Token and cost scores.
    pub cost: CostScores,
    /// Prompt-cache scores.
    pub cache: CacheScores,
    /// Context-management scores.
    pub context: ContextScores,
    /// Memory-recall scores.
    pub memory: MemoryScores,
    /// Tool-use scores.
    pub tools: ToolScores,
    /// Safety counters.
    pub safety: SafetyScores,
    /// Coordination cost: model turns and internal VO round-trips per conversation.
    #[serde(default)]
    pub coordination: CoordinationScores,
}

impl Default for ScoreCard {
    fn default() -> Self {
        Self {
            scenario: String::new(),
            run_id: Uuid::now_v7(),
            timestamp: Utc::now(),
            provider: "recorded".to_string(),
            functional: FunctionalScores::default(),
            latency_ms: LatencyScores::default(),
            cost: CostScores::default(),
            cache: CacheScores::default(),
            context: ContextScores::default(),
            memory: MemoryScores::default(),
            tools: ToolScores::default(),
            safety: SafetyScores::default(),
            coordination: CoordinationScores::default(),
        }
    }
}

impl ScoreCard {
    /// Returns one flat metric row per dashboard score.
    #[must_use]
    pub fn metric_rows(&self) -> Vec<MetricRow> {
        let mut rows = Vec::new();
        push_functional_rows(&mut rows, &self.functional);
        push_latency_rows(&mut rows, &self.latency_ms);
        push_cost_rows(&mut rows, &self.cost);
        push_cache_rows(&mut rows, &self.cache);
        push_context_rows(&mut rows, &self.context);
        push_memory_rows(&mut rows, &self.memory);
        push_tool_rows(&mut rows, &self.tools);
        push_safety_rows(&mut rows, &self.safety);
        push_coordination_rows(&mut rows, &self.coordination);
        rows
    }

    /// Converts metric rows into lineage score records for `analytics.scores`.
    #[must_use]
    pub fn to_score_records(
        &self,
        storage_partition_id: StoragePartitionId,
        user_id: UserId,
        session_id: SessionId,
    ) -> Vec<ScoreRecord> {
        self.metric_rows()
            .into_iter()
            .map(|row| ScoreRecord {
                score_id: Uuid::now_v7(),
                ts: self.timestamp,
                target: ScoreTarget::Session { session_id },
                storage_partition_id: storage_partition_id.clone(),
                user_id: Some(user_id.clone()),
                name: row.name,
                value: lineage_score_value(row.value),
                source: ScoreSource::OfflineReplay,
                model_or_evaluator: format!("long_conversation:{}", self.scenario),
                run_id: Some(self.run_id),
                dataset_id: None,
                comment: Some(format!("provider={}", self.provider)),
                experiment_provenance: None,
            })
            .collect()
    }
}

/// A flat score-card metric row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricRow {
    /// Dot-delimited metric name, such as `cache.input_cached_ratio`.
    pub name: String,
    /// Metric value.
    pub value: Value,
}

/// Functional correctness scores.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FunctionalScores {
    /// Whether a nonblank response was produced and no error event was observed.
    ///
    /// This is a delivery-health signal, not proof that requested work completed.
    pub response_produced_without_error: bool,
    /// Number of user turns driven through the scenario.
    pub turn_count: usize,
    /// Number of error events observed.
    pub error_count: u32,
    /// Whether important errors survived context management.
    pub errors_preserved: bool,
}

impl Default for FunctionalScores {
    fn default() -> Self {
        Self {
            response_produced_without_error: false,
            turn_count: 0,
            error_count: 0,
            errors_preserved: true,
        }
    }
}

/// Latency scores in milliseconds.
///
/// Every field is `None` when the corresponding latency was not measured, rather
/// than copying an aggregate or defaulting to zero. Time-to-first-token is not
/// captured per turn today, so both TTFT fields are always absent; completion
/// percentiles are computed from the real per-turn sample set and are absent when
/// no turn produced a latency sample. An absent completion percentile fails a
/// configured upper-bound latency gate closed instead of passing on a fake zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LatencyScores {
    /// Median submit-to-first-token latency. Absent: TTFT is not measured per turn.
    pub first_token_p50_ms: Option<u64>,
    /// P95 submit-to-first-token latency. Absent: TTFT is not measured per turn.
    pub first_token_p95_ms: Option<u64>,
    /// Median submit-to-completion latency, or `None` when no turn was sampled.
    pub completion_p50_ms: Option<u64>,
    /// P95 submit-to-completion latency, or `None` when no turn was sampled.
    pub completion_p95_ms: Option<u64>,
}

/// Token and cost scores.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CostScores {
    /// Input token count.
    pub input_tokens: usize,
    /// Output token count.
    pub output_tokens: usize,
    /// Cached input token count.
    pub cached_input_tokens: usize,
    /// Rounded final cost in cents.
    pub cost_cents: u32,
}

/// Prompt-cache scores.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheScores {
    /// Fraction of input tokens served from cache.
    pub input_cached_ratio: f64,
    /// Whether stable provider-request prefixes matched across adjacent turns.
    pub prefix_stable: bool,
    /// Longest byte prefix shared across compiled provider requests.
    pub stable_prefix_bytes: usize,
}

/// Context-management scores.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextScores {
    /// Maximum context token count observed.
    pub max_context_tokens: usize,
    /// Number of compaction events observed.
    pub compaction_count: usize,
    /// Number of compaction events observed, using the dashboard's stable metric name.
    pub compaction_events: u32,
    /// Context token count at the first compaction trigger.
    pub tokens_at_first_trigger: u32,
    /// Context token count after compaction.
    pub post_compaction_tokens: u32,
    /// Number of pre-compaction errors preserved after compaction.
    pub errors_preserved: u32,
    /// Number of errors present before the first compaction.
    pub errors_total_pre_compaction: u32,
    /// Whether strict error-preservation checks passed.
    pub errors_preserved_strict: bool,
}

impl Default for ContextScores {
    fn default() -> Self {
        Self {
            max_context_tokens: 0,
            compaction_count: 0,
            compaction_events: 0,
            tokens_at_first_trigger: 0,
            post_compaction_tokens: 0,
            errors_preserved: 0,
            errors_total_pre_compaction: 0,
            errors_preserved_strict: true,
        }
    }
}

/// Memory-recall scores.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryScores {
    /// Planted-fact recall@K.
    pub planted_fact_recall: f64,
    /// Number of memory pages written.
    pub pages_written: usize,
    /// Successful consolidation outcomes.
    pub consolidation_successes: usize,
    /// Failed consolidation outcomes.
    pub consolidation_failures: usize,
}

/// Tool-use scores.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolScores {
    /// Tool calls issued.
    pub tool_call_count: usize,
    /// Tool calls that completed successfully.
    pub tool_success_count: usize,
    /// Tool calls that errored.
    pub tool_error_count: usize,
    /// Successful tool-call fraction.
    pub success_rate: f64,
}

/// Safety counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SafetyScores {
    /// Tool calls that bypassed required approval.
    pub approval_violations: u32,
    /// Canary token leaks into tools.
    pub canary_leaks: u32,
    /// Non-redacted credential exposures.
    pub credential_exposures: u32,
    /// Prompt-injection attempts detected in tool results and blocked.
    pub prompt_injection_attempts_blocked: u32,
    /// Shell chaining attempts blocked from matching an unsafe persisted allow rule.
    pub shell_bypass_attempts_blocked: u32,
}

/// Coordination cost: model turns and internal Restate VO round-trips for one conversation.
///
/// The model-turn and tool-call fields are always meaningful; the VO round-trip fields
/// (`session_vo_calls` … `get_events_calls`) are only populated when the run persisted per-turn
/// `TurnMetrics` (`MOA_PERSIST_TURN_METRICS`), signalled by [`Self::metrics_present`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CoordinationScores {
    /// Model turns to resolve the conversation (fewer = lower latency and cost).
    pub model_turns: u64,
    /// Durable tool calls recorded across the conversation.
    pub total_tool_calls: u64,
    /// Whether per-turn `TurnMetrics` were persisted, so the VO round-trip fields are populated.
    pub metrics_present: bool,
    /// Coordinator↔Session virtual-object round-trips.
    pub session_vo_calls: u64,
    /// Coordinator↔Worker virtual-object round-trips.
    pub worker_vo_calls: u64,
    /// Fire-and-forget virtual-object sends (worker dispatch).
    pub vo_sends: u64,
    /// Durable append steps (replay cost).
    pub durable_appends: u64,
    /// Session event-log reads (replay-read cost).
    pub get_events_calls: u64,
}

impl CoordinationScores {
    /// Builds coordination scores from a reconstructed [`ConversationCost`].
    #[must_use]
    pub fn from_conversation_cost(cost: &ConversationCost) -> Self {
        Self {
            model_turns: cost.model_turns,
            total_tool_calls: cost.total_tool_calls,
            metrics_present: cost.coordination_present,
            session_vo_calls: cost.coordination.session_vo_calls,
            worker_vo_calls: cost.coordination.worker_vo_calls,
            vo_sends: cost.coordination.vo_sends,
            durable_appends: cost.coordination.durable_appends,
            get_events_calls: cost.get_events_calls,
        }
    }

    /// Total internal VO round-trips (session + worker) for the conversation.
    #[must_use]
    pub fn total_vo_round_trips(&self) -> u64 {
        self.session_vo_calls + self.worker_vo_calls
    }
}

fn push_row(rows: &mut Vec<MetricRow>, name: impl Into<String>, value: Value) {
    rows.push(MetricRow {
        name: name.into(),
        value,
    });
}

fn push_functional_rows(rows: &mut Vec<MetricRow>, scores: &FunctionalScores) {
    push_row(
        rows,
        "functional.response_produced_without_error",
        Value::Bool(scores.response_produced_without_error),
    );
    push_row(
        rows,
        "functional.turn_count",
        number(scores.turn_count as u64),
    );
    push_row(
        rows,
        "functional.error_count",
        number(u64::from(scores.error_count)),
    );
    push_row(
        rows,
        "functional.errors_preserved",
        Value::Bool(scores.errors_preserved),
    );
}

fn push_latency_rows(rows: &mut Vec<MetricRow>, scores: &LatencyScores) {
    // Absent latencies emit no row: an unmeasured metric is explicitly missing
    // from the flattened analytics rather than reported as a copied or zero value.
    push_opt_latency_row(
        rows,
        "latency_ms.first_token_p50_ms",
        scores.first_token_p50_ms,
    );
    push_opt_latency_row(
        rows,
        "latency_ms.first_token_p95_ms",
        scores.first_token_p95_ms,
    );
    push_opt_latency_row(
        rows,
        "latency_ms.completion_p50_ms",
        scores.completion_p50_ms,
    );
    push_opt_latency_row(
        rows,
        "latency_ms.completion_p95_ms",
        scores.completion_p95_ms,
    );
}

fn push_opt_latency_row(rows: &mut Vec<MetricRow>, name: &str, value: Option<u64>) {
    if let Some(value) = value {
        push_row(rows, name, number(value));
    }
}

fn push_cost_rows(rows: &mut Vec<MetricRow>, scores: &CostScores) {
    push_row(
        rows,
        "cost.input_tokens",
        number(scores.input_tokens as u64),
    );
    push_row(
        rows,
        "cost.output_tokens",
        number(scores.output_tokens as u64),
    );
    push_row(
        rows,
        "cost.cached_input_tokens",
        number(scores.cached_input_tokens as u64),
    );
    push_row(
        rows,
        "cost.cost_cents",
        number(u64::from(scores.cost_cents)),
    );
}

fn push_cache_rows(rows: &mut Vec<MetricRow>, scores: &CacheScores) {
    push_row(
        rows,
        "cache.input_cached_ratio",
        float_number(scores.input_cached_ratio),
    );
    push_row(
        rows,
        "cache.prefix_stable",
        Value::Bool(scores.prefix_stable),
    );
    push_row(
        rows,
        "cache.stable_prefix_bytes",
        number(scores.stable_prefix_bytes as u64),
    );
}

fn push_context_rows(rows: &mut Vec<MetricRow>, scores: &ContextScores) {
    push_row(
        rows,
        "context.max_context_tokens",
        number(scores.max_context_tokens as u64),
    );
    push_row(
        rows,
        "context.compaction_count",
        number(scores.compaction_count as u64),
    );
    push_row(
        rows,
        "context.compaction_events",
        number(u64::from(scores.compaction_events)),
    );
    push_row(
        rows,
        "context.tokens_at_first_trigger",
        number(u64::from(scores.tokens_at_first_trigger)),
    );
    push_row(
        rows,
        "context.post_compaction_tokens",
        number(u64::from(scores.post_compaction_tokens)),
    );
    push_row(
        rows,
        "context.errors_preserved",
        number(u64::from(scores.errors_preserved)),
    );
    push_row(
        rows,
        "context.errors_total_pre_compaction",
        number(u64::from(scores.errors_total_pre_compaction)),
    );
    push_row(
        rows,
        "context.errors_preserved_strict",
        Value::Bool(scores.errors_preserved_strict),
    );
}

fn push_memory_rows(rows: &mut Vec<MetricRow>, scores: &MemoryScores) {
    push_row(
        rows,
        "memory.planted_fact_recall",
        float_number(scores.planted_fact_recall),
    );
    push_row(
        rows,
        "memory.pages_written",
        number(scores.pages_written as u64),
    );
    push_row(
        rows,
        "memory.consolidation_successes",
        number(scores.consolidation_successes as u64),
    );
    push_row(
        rows,
        "memory.consolidation_failures",
        number(scores.consolidation_failures as u64),
    );
}

fn push_tool_rows(rows: &mut Vec<MetricRow>, scores: &ToolScores) {
    push_row(
        rows,
        "tools.tool_call_count",
        number(scores.tool_call_count as u64),
    );
    push_row(
        rows,
        "tools.tool_success_count",
        number(scores.tool_success_count as u64),
    );
    push_row(
        rows,
        "tools.tool_error_count",
        number(scores.tool_error_count as u64),
    );
    push_row(
        rows,
        "tools.success_rate",
        float_number(scores.success_rate),
    );
}

fn push_safety_rows(rows: &mut Vec<MetricRow>, scores: &SafetyScores) {
    push_row(
        rows,
        "safety.approval_violations",
        number(u64::from(scores.approval_violations)),
    );
    push_row(
        rows,
        "safety.canary_leaks",
        number(u64::from(scores.canary_leaks)),
    );
    push_row(
        rows,
        "safety.credential_exposures",
        number(u64::from(scores.credential_exposures)),
    );
    push_row(
        rows,
        "safety.prompt_injection_attempts_blocked",
        number(u64::from(scores.prompt_injection_attempts_blocked)),
    );
    push_row(
        rows,
        "safety.shell_bypass_attempts_blocked",
        number(u64::from(scores.shell_bypass_attempts_blocked)),
    );
}

fn push_coordination_rows(rows: &mut Vec<MetricRow>, scores: &CoordinationScores) {
    push_row(rows, "coordination.model_turns", number(scores.model_turns));
    push_row(
        rows,
        "coordination.total_tool_calls",
        number(scores.total_tool_calls),
    );
    push_row(
        rows,
        "coordination.metrics_present",
        Value::Bool(scores.metrics_present),
    );
    push_row(
        rows,
        "coordination.session_vo_calls",
        number(scores.session_vo_calls),
    );
    push_row(
        rows,
        "coordination.worker_vo_calls",
        number(scores.worker_vo_calls),
    );
    push_row(rows, "coordination.vo_sends", number(scores.vo_sends));
    push_row(
        rows,
        "coordination.durable_appends",
        number(scores.durable_appends),
    );
    push_row(
        rows,
        "coordination.get_events_calls",
        number(scores.get_events_calls),
    );
    push_row(
        rows,
        "coordination.total_vo_round_trips",
        number(scores.total_vo_round_trips()),
    );
}

fn number(value: u64) -> Value {
    Value::Number(Number::from(value))
}

fn float_number(value: f64) -> Value {
    Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or_else(|| Value::Number(Number::from(0_u64)))
}

fn lineage_score_value(value: Value) -> LineageScoreValue {
    match value {
        Value::Bool(value) => LineageScoreValue::Boolean(value),
        Value::Number(value) => LineageScoreValue::Numeric(value.as_f64().unwrap_or(0.0)),
        Value::String(value) => LineageScoreValue::Categorical(value),
        other => LineageScoreValue::Categorical(other.to_string()),
    }
}

//! Typed analytics read-model DTOs shared by session storage and API surfaces.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{ContactId, SessionId, SessionStatus, TenantId};

/// One session-level analytics row sourced from the `session_summary` view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionAnalyticsSummary {
    /// Session identifier.
    pub session_id: SessionId,
    /// Tenant identifier.
    pub tenant_id: TenantId,
    /// Contact identifier, when the session is contact-backed.
    #[serde(default)]
    pub contact_id: Option<ContactId>,
    /// Current persisted session status.
    pub status: SessionStatus,
    /// Number of completed assistant turns.
    pub turn_count: u64,
    /// Total event count for the session.
    pub event_count: u64,
    /// Total input tokens across cached and uncached paths.
    pub total_input_tokens: u64,
    /// Total output tokens.
    pub total_output_tokens: u64,
    /// Total session cost in cents.
    pub total_cost_cents: u64,
    /// Total main-loop cost in cents.
    pub main_cost_cents: u64,
    /// Total auxiliary-tier cost in cents.
    pub auxiliary_cost_cents: u64,
    /// Fraction of input tokens served from cache.
    pub cache_hit_rate: f64,
    /// Session wall-clock duration in seconds.
    pub duration_seconds: f64,
    /// Number of tool calls recorded for the session.
    pub tool_call_count: u64,
    /// Number of error events recorded for the session.
    pub error_count: u64,
}

/// One per-tool analytics row sourced from `tool_call_summary` or `tool_call_analytics`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallSummary {
    /// Stable tool name.
    pub tool_name: String,
    /// Number of completed calls for the tool.
    pub call_count: u64,
    /// Mean duration in milliseconds.
    pub avg_duration_ms: f64,
    /// Median duration in milliseconds.
    pub p50_ms: f64,
    /// P95 duration in milliseconds.
    pub p95_ms: f64,
    /// Fraction of calls that succeeded.
    pub success_rate: f64,
}

/// One per-turn analytics row sourced from the `session_turn_metrics` materialized view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionTurnMetric {
    /// Session identifier.
    pub session_id: SessionId,
    /// Tenant identifier.
    pub tenant_id: TenantId,
    /// Contact identifier, when the turn is contact-backed.
    pub contact_id: Option<ContactId>,
    /// One-based turn number within the session.
    pub turn_number: u64,
    /// Timestamp when the assistant turn completed.
    pub finished_at: DateTime<Utc>,
    /// Model recorded for the turn.
    pub model: String,
    /// Pipeline duration when available.
    pub pipeline_ms: Option<f64>,
    /// Provider response duration in milliseconds.
    pub llm_ms: f64,
    /// Aggregate tool execution duration in milliseconds for the turn.
    pub tool_ms: f64,
    /// Number of tool calls in the turn.
    pub tool_call_count: u64,
    /// Uncached input tokens for the turn.
    pub input_tokens_uncached: u64,
    /// Cache-write input tokens for the turn.
    pub input_tokens_cache_write: u64,
    /// Cache-read input tokens for the turn.
    pub input_tokens_cache_read: u64,
    /// Total input tokens for the turn.
    pub total_input_tokens: u64,
    /// Output tokens for the turn.
    pub output_tokens: u64,
    /// Turn cost in cents.
    pub cost_cents: u64,
}

/// Aggregate tenant metrics over a bounded recent time window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TenantAnalyticsSummary {
    /// Tenant identifier.
    pub tenant_id: TenantId,
    /// Number of whole days included in the rollup window.
    pub days: u32,
    /// Session count across the window.
    pub session_count: u64,
    /// Turn count across the window.
    pub turn_count: u64,
    /// Total input tokens across the window.
    pub total_input_tokens: u64,
    /// Cache-read input tokens across the window.
    pub total_cache_read_tokens: u64,
    /// Total output tokens across the window.
    pub total_output_tokens: u64,
    /// Total cost in cents across the window.
    pub total_cost_cents: u64,
    /// Weighted cache-hit rate for the window.
    pub cache_hit_rate: f64,
}

/// One daily cache trend point sourced from `daily_storage_partition_metrics`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheDailyMetric {
    /// Tenant identifier.
    pub tenant_id: TenantId,
    /// UTC day bucket.
    pub day: DateTime<Utc>,
    /// Session count on the day.
    pub session_count: u64,
    /// Turn count on the day.
    pub turn_count: u64,
    /// Total input tokens on the day.
    pub total_input_tokens: u64,
    /// Total cache-read tokens on the day.
    pub total_cache_read_tokens: u64,
    /// Total output tokens on the day.
    pub total_output_tokens: u64,
    /// Total cost in cents on the day.
    pub total_cost_cents: u64,
    /// Average cache-hit rate on the day.
    pub avg_cache_hit_rate: f64,
}

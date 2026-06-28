//! Analytics service wire DTOs.

use crate::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Request payload for reading analytics for one session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStatsRequest {
    /// Session whose analytics summary should be read.
    pub session_id: SessionId,
}

/// Response payload containing one session analytics summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStatsResponse {
    /// Session identifier.
    pub session_id: SessionId,
    /// Tenant that owns the session.
    pub tenant_id: TenantId,
    /// Contact attached to the session, when any.
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

/// Request payload for reading tenant analytics over a recent window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TenantStatsRequest {
    /// Tenant whose rollup should be read.
    pub tenant_id: TenantId,
    /// Number of whole days included in the rollup window.
    pub days: u32,
}

/// Response payload containing tenant analytics over a recent window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TenantStatsResponse {
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

/// Request payload for reading per-tool analytics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolStatsRequest {
    /// Optional tenant filter for the per-tool rollup.
    ///
    /// Non-service callers are forced to their authenticated tenant by the
    /// edge; service callers may omit this for deployment-wide stats.
    pub tenant_id: Option<TenantId>,
}

/// Response payload containing per-tool analytics rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolStatsResponse {
    /// Tenant filter used for this response, if one was requested.
    pub tenant_id: Option<TenantId>,
    /// Per-tool analytics rows ordered for API display.
    #[serde(default)]
    pub rows: Vec<ToolStatsRow>,
}

/// One per-tool analytics row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolStatsRow {
    /// Stable tool name.
    pub tool_name: String,
    /// Number of completed calls for the tool.
    pub call_count: u64,
    /// Fraction of calls that succeeded.
    pub success_rate: f64,
    /// Mean duration in milliseconds.
    pub avg_duration_ms: f64,
    /// Median duration in milliseconds.
    pub p50_ms: f64,
    /// P95 duration in milliseconds.
    pub p95_ms: f64,
}

/// Request payload for reading tenant cache analytics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheStatsRequest {
    /// Tenant whose cache rollup should be read.
    pub tenant_id: TenantId,
    /// Number of whole days included in the cache window.
    pub days: u32,
}

/// Response payload containing tenant cache analytics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheStatsResponse {
    /// Tenant identifier.
    pub tenant_id: TenantId,
    /// Number of whole days included in the cache window.
    pub days: u32,
    /// Weighted cache-hit rate for the window.
    pub cache_hit_rate: f64,
    /// Cache-read input tokens across the window.
    pub total_cache_read_tokens: u64,
    /// Total input tokens across the window.
    pub total_input_tokens: u64,
    /// Total output tokens across the window.
    pub total_output_tokens: u64,
    /// Total cost in cents across the window.
    pub total_cost_cents: u64,
    /// Estimated cache savings in cents when pricing history can support it.
    pub estimated_savings_cents: Option<u64>,
    /// Daily cache trend rows ordered by day.
    #[serde(default)]
    pub daily: Vec<CacheDailyMetricRow>,
}

/// One daily tenant cache trend point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheDailyMetricRow {
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

/// Request payload for tenant-scoped live experiment analytics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentAnalyticsRequest {
    /// Tenant whose experiment runs should be summarized.
    pub tenant_id: TenantId,
    /// Optional lower bound on experiment creation time.
    pub from_time: Option<DateTime<Utc>>,
    /// Optional upper bound on experiment creation time.
    pub to_time: Option<DateTime<Utc>>,
    /// Maximum number of score-run references to include.
    pub limit: u32,
}

/// Response payload containing tenant-scoped experiment analytics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentAnalyticsResponse {
    /// Tenant whose experiment runs were summarized.
    pub tenant_id: TenantId,
    /// Total experiment runs in the requested window.
    pub total_runs: u64,
    /// Per-status run counts ordered by status.
    #[serde(default)]
    pub statuses: Vec<ExperimentStatusCount>,
    /// Score-run references ordered by newest experiment run first.
    #[serde(default)]
    pub score_runs: Vec<ExperimentScoreRunRef>,
    /// Daily experiment run trend points.
    #[serde(default)]
    pub run_trends: Vec<ExperimentRunTrendPoint>,
    /// Daily experiment trial trend points.
    #[serde(default)]
    pub trial_trends: Vec<ExperimentTrialTrendPoint>,
}

/// Count of experiment runs for one lifecycle status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentStatusCount {
    /// Durable experiment run status.
    pub status: String,
    /// Number of runs with this status.
    pub count: u64,
}

/// Reference from an experiment run to the associated score run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentScoreRunRef {
    /// Stable experiment run identifier.
    pub run_uid: Uuid,
    /// Human-readable experiment run name.
    pub name: String,
    /// Durable experiment run status.
    pub status: String,
    /// Score run identifier used by `analytics.scores`.
    pub score_run_id: Uuid,
    /// Time the experiment run was accepted.
    pub created_at: DateTime<Utc>,
}

/// Daily count of experiment runs for one lifecycle status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentRunTrendPoint {
    /// UTC day bucket.
    pub day: DateTime<Utc>,
    /// Durable experiment run status.
    pub status: String,
    /// Number of runs created in the day bucket with this status.
    pub count: u64,
}

/// Daily count of experiment trials for one lifecycle status and matrix cell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentTrialTrendPoint {
    /// UTC day bucket.
    pub day: DateTime<Utc>,
    /// Durable experiment trial status.
    pub status: String,
    /// Stable target variant key selected for the trial.
    pub variant_key: String,
    /// Stable scenario ID selected for the trial.
    pub scenario_id: Option<String>,
    /// Number of trials created in the day bucket with this status and matrix cell.
    pub count: u64,
}

/// Request payload for listing curated learning-candidate summaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningCandidateListRequest {
    /// Tenant whose candidates should be listed.
    pub tenant_id: TenantId,
    /// Optional candidate status filter.
    pub status: Option<LearningCandidateStatus>,
    /// Maximum number of candidates to return.
    pub limit: u32,
}

/// Response payload containing curated learning-candidate summaries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearningCandidateListResponse {
    /// Tenant scope used for this response.
    pub tenant_id: TenantId,
    /// Candidate summaries ordered by newest update first.
    #[serde(default)]
    pub candidates: Vec<LearningCandidateSummary>,
}

/// Redacted read-model projection of one learning candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearningCandidateSummary {
    /// Stable candidate identifier.
    pub id: Uuid,
    /// Tenant scope for the candidate.
    pub tenant_id: TenantId,
    /// Optional contact scope for contact-local candidates.
    pub contact_id: Option<ContactId>,
    /// Candidate target type.
    pub candidate_type: LearningCandidateType,
    /// Current promotion status.
    pub status: LearningCandidateStatus,
    /// Optional target identifier when mutating existing learned state.
    pub target_id: Option<String>,
    /// Optional human-readable target label.
    pub target_label: Option<String>,
    /// Task fingerprint hash the candidate is expected to help.
    pub task_fingerprint: Option<String>,
    /// Confidence in the candidate proposal.
    pub confidence: Option<f64>,
    /// Promotion risk class.
    pub risk_class: LearningRiskClass,
    /// Short, redacted preview of the candidate payload.
    pub payload_preview: String,
    /// Candidate creation time.
    pub created_at: DateTime<Utc>,
    /// Last candidate update time.
    pub updated_at: DateTime<Utc>,
}

/// Request payload for tenant-scoped session event search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSearchRequest {
    /// Tenant whose sessions should be searched.
    pub tenant_id: TenantId,
    /// Full-text event search query.
    pub query: String,
    /// Optional lower timestamp bound.
    pub from_time: Option<DateTime<Utc>>,
    /// Optional upper timestamp bound.
    pub to_time: Option<DateTime<Utc>>,
    /// Optional event type filter.
    pub event_types: Option<Vec<EventType>>,
    /// Maximum number of snippets to return.
    pub limit: u32,
}

/// Response payload containing redacted session event snippets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSearchResponse {
    /// Tenant whose sessions were searched.
    pub tenant_id: TenantId,
    /// Query text that produced the results.
    pub query: String,
    /// Redacted event snippets ordered by search rank.
    #[serde(default)]
    pub results: Vec<SessionSearchResult>,
}

/// One redacted event-search hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSearchResult {
    /// Session that owns the matching event.
    pub session_id: SessionId,
    /// Stable event identifier.
    pub event_id: Uuid,
    /// Event sequence number within the session.
    pub sequence_num: u64,
    /// Event type discriminator.
    pub event_type: EventType,
    /// Time the event was emitted.
    pub timestamp: DateTime<Utc>,
    /// Short redacted snippet for analytics review.
    pub snippet: String,
}

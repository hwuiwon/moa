//! Workspace-scoped tool performance tracking and schema ranking helpers.

#[cfg(test)]
use std::cmp::Ordering;
use std::collections::HashMap;

use chrono::{DateTime, Utc};
#[cfg(test)]
use moa_core::{Event, EventRecord, ToolCallId};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use serde_json::Value;

#[cfg(test)]
const TOOL_STATS_EMA_ALPHA: f64 = 0.1;
#[cfg(test)]
const TOOL_RANKING_MIN_CALLS: u64 = 5;
#[cfg(test)]
const TOOL_ANNOTATION_MIN_CALLS: u64 = 10;
#[cfg(test)]
const TOOL_WARNING_SUCCESS_THRESHOLD: f64 = 0.8;
#[cfg(test)]
const MAX_COMMON_ERRORS: usize = 3;

/// Aggregate historical performance for one tool in one workspace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolStats {
    /// Stable tool name.
    pub tool_name: String,
    /// Total recorded calls.
    pub total_calls: u64,
    /// Total successful calls.
    pub successes: u64,
    /// Total failed calls.
    pub failures: u64,
    /// Smoothed average duration in milliseconds for completed executions.
    pub avg_duration_ms: f64,
    /// Most common normalized error patterns and their counts.
    pub common_errors: Vec<(String, u32)>,
    /// When the tool was last used in this workspace.
    pub last_used: DateTime<Utc>,
    /// Exponential moving average of session-level success rate.
    pub ema_success_rate: f64,
    /// Optional human-authored or retained workspace tips.
    pub workspace_tips: Vec<String>,
}

impl Default for ToolStats {
    fn default() -> Self {
        Self {
            tool_name: String::new(),
            total_calls: 0,
            successes: 0,
            failures: 0,
            avg_duration_ms: 0.0,
            common_errors: Vec::new(),
            last_used: Utc::now(),
            ema_success_rate: 1.0,
            workspace_tips: Vec::new(),
        }
    }
}

/// Workspace-wide tool statistics persisted in workspace memory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceToolStats {
    /// Per-tool performance aggregates keyed by tool name.
    pub tools: HashMap<String, ToolStats>,
    /// Last time the stats page was refreshed.
    pub last_updated: DateTime<Utc>,
    /// Number of sessions incorporated into this snapshot.
    pub sessions_tracked: u64,
}

impl Default for WorkspaceToolStats {
    fn default() -> Self {
        Self {
            tools: HashMap::new(),
            last_updated: Utc::now(),
            sessions_tracked: 0,
        }
    }
}

#[cfg(test)]
#[derive(Debug, Default)]
struct SessionToolObservation {
    total_calls: u64,
    successes: u64,
    failures: u64,
    total_duration_ms: u64,
    duration_samples: u64,
    common_errors: HashMap<String, u32>,
    last_used: Option<DateTime<Utc>>,
}

/// Updates an exponential moving average from one new observation.
pub fn update_ema(current: f64, observation: f64, alpha: f64) -> f64 {
    alpha * observation + (1.0 - alpha) * current
}

#[cfg(test)]
fn workspace_tool_stats_from_events(events: &[EventRecord]) -> Option<WorkspaceToolStats> {
    let observations = collect_session_tool_observations(events);
    if observations.is_empty() {
        return None;
    }

    let mut stats = WorkspaceToolStats::default();
    for (tool_name, observation) in observations {
        merge_session_observation(
            stats
                .tools
                .entry(tool_name.clone())
                .or_insert_with(|| ToolStats {
                    tool_name,
                    last_used: observation.last_used.unwrap_or_else(Utc::now),
                    ..ToolStats::default()
                }),
            observation,
        );
    }
    stats.last_updated = Utc::now();
    stats.sessions_tracked = stats.sessions_tracked.saturating_add(1);

    Some(stats)
}

#[cfg(test)]
pub(crate) fn apply_tool_rankings(
    mut tool_schemas: Vec<Value>,
    stats: &WorkspaceToolStats,
) -> Vec<Value> {
    if stats.tools.is_empty() {
        return tool_schemas;
    }

    tool_schemas.sort_by(|left, right| compare_schemas(left, right, stats));
    for schema in &mut tool_schemas {
        annotate_schema(schema, stats);
    }

    tool_schemas
}

#[cfg(test)]
fn compare_schemas(left: &Value, right: &Value, stats: &WorkspaceToolStats) -> Ordering {
    let left_name = schema_name(left);
    let right_name = schema_name(right);
    let left_stats = left_name.and_then(|name| stats.tools.get(name));
    let right_stats = right_name.and_then(|name| stats.tools.get(name));
    let left_tier = tool_rank_tier(left_stats);
    let right_tier = tool_rank_tier(right_stats);

    left_tier
        .cmp(&right_tier)
        .then_with(|| compare_within_tier(left_stats, right_stats, left_tier))
        .then_with(|| left_name.cmp(&right_name))
}

#[cfg(test)]
fn compare_within_tier(left: Option<&ToolStats>, right: Option<&ToolStats>, tier: u8) -> Ordering {
    match tier {
        0 => compare_success_first(left, right),
        2 => compare_failure_last(left, right),
        _ => Ordering::Equal,
    }
}

#[cfg(test)]
fn compare_success_first(left: Option<&ToolStats>, right: Option<&ToolStats>) -> Ordering {
    compare_f64_desc(
        left.map(|stats| stats.ema_success_rate).unwrap_or_default(),
        right
            .map(|stats| stats.ema_success_rate)
            .unwrap_or_default(),
    )
    .then_with(|| {
        right
            .map(|stats| stats.total_calls)
            .cmp(&left.map(|stats| stats.total_calls))
    })
}

#[cfg(test)]
fn compare_failure_last(left: Option<&ToolStats>, right: Option<&ToolStats>) -> Ordering {
    compare_f64_asc(
        left.map(|stats| stats.ema_success_rate).unwrap_or(1.0),
        right.map(|stats| stats.ema_success_rate).unwrap_or(1.0),
    )
    .then_with(|| {
        right
            .map(|stats| stats.total_calls)
            .cmp(&left.map(|stats| stats.total_calls))
    })
}

#[cfg(test)]
fn compare_f64_desc(left: f64, right: f64) -> Ordering {
    right.partial_cmp(&left).unwrap_or(Ordering::Equal)
}

#[cfg(test)]
fn compare_f64_asc(left: f64, right: f64) -> Ordering {
    left.partial_cmp(&right).unwrap_or(Ordering::Equal)
}

#[cfg(test)]
fn tool_rank_tier(stats: Option<&ToolStats>) -> u8 {
    match stats {
        Some(stats)
            if stats.total_calls >= TOOL_RANKING_MIN_CALLS
                && stats.ema_success_rate >= TOOL_WARNING_SUCCESS_THRESHOLD =>
        {
            0
        }
        Some(stats) if stats.total_calls >= TOOL_RANKING_MIN_CALLS => 2,
        _ => 1,
    }
}

#[cfg(test)]
fn annotate_schema(schema: &mut Value, stats: &WorkspaceToolStats) {
    let Some(name) = schema_name(schema) else {
        return;
    };
    let Some(tool_stats) = stats.tools.get(name) else {
        return;
    };
    let Some(description) = schema
        .get("description")
        .and_then(Value::as_str)
        .map(ToString::to_string)
    else {
        return;
    };
    let Some(annotation) = tool_annotation(tool_stats) else {
        return;
    };

    if let Some(object) = schema.as_object_mut() {
        object.insert(
            "description".to_string(),
            Value::String(format!("{description}\n\n{annotation}")),
        );
    }
}

#[cfg(test)]
fn tool_annotation(stats: &ToolStats) -> Option<String> {
    let mut notes = Vec::new();
    if stats.total_calls >= TOOL_ANNOTATION_MIN_CALLS {
        let duration_note = if stats.avg_duration_ms > 0.0 {
            format!(", avg {}", format_duration(stats.avg_duration_ms))
        } else {
            String::new()
        };
        notes.push(format!(
            "[Workspace note: {} success{}.]",
            format_percentage(stats.ema_success_rate),
            duration_note
        ));
        if failure_rate(stats) >= (1.0 - TOOL_WARNING_SUCCESS_THRESHOLD) {
            if let Some((pattern, _)) = stats.common_errors.first() {
                notes.push(format!("[Workspace warning: common failure: {}.]", pattern));
            } else {
                notes
                    .push("[Workspace warning: this tool has failed frequently here.]".to_string());
            }
        }
    }

    for tip in &stats.workspace_tips {
        if notes.len() >= 2 {
            break;
        }
        let trimmed = tip.trim();
        if trimmed.is_empty() {
            continue;
        }
        notes.push(format!("[Workspace tip: {}]", trimmed));
    }

    if notes.is_empty() {
        None
    } else {
        Some(notes.join("\n"))
    }
}

#[cfg(test)]
fn format_percentage(value: f64) -> String {
    format!("{:.0}%", (value.clamp(0.0, 1.0) * 100.0).round())
}

#[cfg(test)]
fn format_duration(duration_ms: f64) -> String {
    if duration_ms >= 1000.0 {
        format!("{:.1}s", duration_ms / 1000.0)
    } else {
        format!("{duration_ms:.0}ms")
    }
}

#[cfg(test)]
fn failure_rate(stats: &ToolStats) -> f64 {
    if stats.total_calls == 0 {
        0.0
    } else {
        stats.failures as f64 / stats.total_calls as f64
    }
}

#[cfg(test)]
fn schema_name(schema: &Value) -> Option<&str> {
    schema.get("name").and_then(Value::as_str)
}

#[cfg(test)]
fn collect_session_tool_observations(
    events: &[EventRecord],
) -> HashMap<String, SessionToolObservation> {
    let mut call_names = HashMap::<ToolCallId, String>::new();
    let mut observations = HashMap::<String, SessionToolObservation>::new();

    for record in events {
        match &record.event {
            Event::ToolCall {
                tool_id, tool_name, ..
            } => {
                call_names.insert(*tool_id, tool_name.clone());
            }
            Event::ToolResult {
                tool_id,
                output,
                success,
                duration_ms,
                ..
            } => {
                let Some(tool_name) = call_names.get(tool_id).cloned() else {
                    continue;
                };
                let observation = observations.entry(tool_name).or_default();
                observation.total_calls = observation.total_calls.saturating_add(1);
                observation.total_duration_ms =
                    observation.total_duration_ms.saturating_add(*duration_ms);
                observation.duration_samples = observation.duration_samples.saturating_add(1);
                observation.last_used = Some(record.timestamp);
                if *success {
                    observation.successes = observation.successes.saturating_add(1);
                } else {
                    observation.failures = observation.failures.saturating_add(1);
                    record_error_pattern(&mut observation.common_errors, &output.to_text());
                }
            }
            Event::ToolError {
                tool_id,
                tool_name,
                error,
                ..
            } => {
                let resolved_name = if tool_name.is_empty() {
                    call_names.get(tool_id).cloned()
                } else {
                    Some(tool_name.clone())
                };
                let Some(tool_name) = resolved_name else {
                    continue;
                };
                let observation = observations.entry(tool_name).or_default();
                observation.total_calls = observation.total_calls.saturating_add(1);
                observation.failures = observation.failures.saturating_add(1);
                observation.last_used = Some(record.timestamp);
                record_error_pattern(&mut observation.common_errors, error);
            }
            _ => {}
        }
    }

    observations
}

#[cfg(test)]
fn merge_session_observation(stats: &mut ToolStats, observation: SessionToolObservation) {
    let previous_calls = stats.total_calls;
    stats.total_calls = stats.total_calls.saturating_add(observation.total_calls);
    stats.successes = stats.successes.saturating_add(observation.successes);
    stats.failures = stats.failures.saturating_add(observation.failures);
    if let Some(last_used) = observation.last_used {
        stats.last_used = last_used;
    }

    if observation.total_calls > 0 {
        let session_success_rate = observation.successes as f64 / observation.total_calls as f64;
        stats.ema_success_rate = if previous_calls == 0 {
            session_success_rate
        } else {
            update_ema(
                stats.ema_success_rate,
                session_success_rate,
                TOOL_STATS_EMA_ALPHA,
            )
        };
    }

    if observation.duration_samples > 0 {
        let observed_avg =
            observation.total_duration_ms as f64 / observation.duration_samples as f64;
        stats.avg_duration_ms = if previous_calls == 0 || stats.avg_duration_ms <= 0.0 {
            observed_avg
        } else {
            update_ema(stats.avg_duration_ms, observed_avg, TOOL_STATS_EMA_ALPHA)
        };
    }

    let mut combined = stats
        .common_errors
        .iter()
        .cloned()
        .collect::<HashMap<String, u32>>();
    for (pattern, count) in observation.common_errors {
        *combined.entry(pattern).or_insert(0) += count;
    }
    stats.common_errors = top_error_patterns(combined);
}

#[cfg(test)]
fn top_error_patterns(patterns: HashMap<String, u32>) -> Vec<(String, u32)> {
    let mut entries = patterns.into_iter().collect::<Vec<_>>();
    entries.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    entries.truncate(MAX_COMMON_ERRORS);
    entries
}

#[cfg(test)]
fn record_error_pattern(errors: &mut HashMap<String, u32>, raw: &str) {
    let normalized = normalize_error_pattern(raw);
    if normalized.is_empty() {
        return;
    }
    *errors.entry(normalized).or_insert(0) += 1;
}

#[cfg(test)]
fn normalize_error_pattern(raw: &str) -> String {
    let first_line = raw
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let normalized = first_line
        .strip_prefix("provider error: ")
        .or_else(|| first_line.strip_prefix("tool error: "))
        .unwrap_or(first_line)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    truncate_with_ellipsis(&normalized, 96)
}

#[cfg(test)]
fn truncate_with_ellipsis(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }

    let mut truncated = value
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    truncated.push_str("...");
    truncated
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::Duration;
    use moa_core::{
        ModelId, Platform, SessionId, SessionMeta, ToolContent, ToolOutput, UserId, WorkspaceId,
    };
    use uuid::Uuid;

    use super::*;

    #[test]
    fn ranking_puts_successful_tools_first() {
        let stats = WorkspaceToolStats {
            tools: HashMap::from([
                (
                    "bash".to_string(),
                    ToolStats {
                        tool_name: "bash".to_string(),
                        total_calls: 20,
                        ema_success_rate: 0.95,
                        ..ToolStats::default()
                    },
                ),
                (
                    "file_read".to_string(),
                    ToolStats {
                        tool_name: "file_read".to_string(),
                        total_calls: 20,
                        ema_success_rate: 0.99,
                        ..ToolStats::default()
                    },
                ),
                (
                    "web_search".to_string(),
                    ToolStats {
                        tool_name: "web_search".to_string(),
                        total_calls: 20,
                        ema_success_rate: 0.60,
                        ..ToolStats::default()
                    },
                ),
            ]),
            ..WorkspaceToolStats::default()
        };
        let ranked = apply_tool_rankings(
            vec![
                serde_json::json!({"name": "web_search", "description": "search"}),
                serde_json::json!({"name": "bash", "description": "shell"}),
                serde_json::json!({"name": "file_read", "description": "read"}),
            ],
            &stats,
        );

        let names = ranked
            .iter()
            .map(|schema| schema["name"].as_str().expect("tool name"))
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["file_read", "bash", "web_search"]);
    }

    #[test]
    fn annotation_warns_on_low_success() {
        let stats = ToolStats {
            tool_name: "web_search".to_string(),
            total_calls: 12,
            failures: 5,
            ema_success_rate: 0.5,
            common_errors: vec![("timeout".to_string(), 3)],
            ..ToolStats::default()
        };

        let annotation = tool_annotation(&stats).expect("annotation");
        assert!(annotation.contains("Workspace warning"));
        assert!(annotation.contains("timeout"));
    }

    #[test]
    fn ema_decays_old_failures() {
        let mut value = 0.0;
        for _ in 0..7 {
            value = update_ema(value, 1.0, TOOL_STATS_EMA_ALPHA);
        }

        assert!(value > 0.5);
    }

    #[test]
    fn no_annotation_below_threshold() {
        let stats = ToolStats {
            tool_name: "bash".to_string(),
            total_calls: 3,
            ema_success_rate: 1.0,
            ..ToolStats::default()
        };

        assert_eq!(tool_annotation(&stats), None);
    }

    #[tokio::test]
    async fn stats_update_from_events() {
        let session = SessionMeta {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new("ws-stats"),
            user_id: UserId::new("user"),
            platform: Platform::Desktop,
            model: ModelId::new("claude-sonnet-4-6"),
            ..SessionMeta::default()
        };
        let now = Utc::now();
        let tool_id = ToolCallId::new();
        let events = vec![
            event_record(
                &session,
                1,
                now,
                Event::ToolCall {
                    tool_id,
                    provider_tool_use_id: None,
                    provider_thought_signature: None,
                    tool_name: "bash".to_string(),
                    input: serde_json::json!({"cmd": "npm test"}),
                    hand_id: None,
                },
            ),
            event_record(
                &session,
                2,
                now + Duration::seconds(1),
                Event::ToolResult {
                    tool_id,
                    provider_tool_use_id: None,
                    output: ToolOutput {
                        content: vec![ToolContent::Text {
                            text: "ok".to_string(),
                        }],
                        is_error: false,
                        structured: None,
                        duration: std::time::Duration::from_secs(2),
                        truncated: false,
                        original_output_tokens: None,
                        artifact: None,
                    },
                    original_output_tokens: None,
                    success: true,
                    duration_ms: 2000,
                },
            ),
            event_record(
                &session,
                3,
                now + Duration::seconds(2),
                Event::ToolError {
                    tool_id: ToolCallId::new(),
                    provider_tool_use_id: None,
                    tool_name: "web_search".to_string(),
                    error: "provider error: timeout".to_string(),
                    retryable: false,
                },
            ),
        ];

        let stats = workspace_tool_stats_from_events(&events).expect("stats");

        assert_eq!(stats.sessions_tracked, 1);
        assert_eq!(stats.tools["bash"].successes, 1);
        assert_eq!(stats.tools["web_search"].failures, 1);
    }

    #[tokio::test]
    async fn cache_stability_preserves_identical_ranked_output() {
        let stats = WorkspaceToolStats {
            tools: HashMap::from([(
                "bash".to_string(),
                ToolStats {
                    tool_name: "bash".to_string(),
                    total_calls: 12,
                    ema_success_rate: 0.95,
                    ..ToolStats::default()
                },
            )]),
            ..WorkspaceToolStats::default()
        };

        let first = serde_json::to_string(&apply_tool_rankings(
            vec![
                serde_json::json!({"name": "bash", "description": "shell"}),
                serde_json::json!({"name": "web_search", "description": "search"}),
            ],
            &stats,
        ))
        .expect("first serialization");
        let second = serde_json::to_string(&apply_tool_rankings(
            vec![
                serde_json::json!({"name": "bash", "description": "shell"}),
                serde_json::json!({"name": "web_search", "description": "search"}),
            ],
            &stats,
        ))
        .expect("second serialization");

        assert_eq!(first, second);
    }

    fn event_record(
        session: &SessionMeta,
        sequence_num: u64,
        timestamp: DateTime<Utc>,
        event: Event,
    ) -> EventRecord {
        EventRecord {
            id: Uuid::now_v7(),
            session_id: session.id,
            sequence_num,
            event_type: event.event_type(),
            event,
            timestamp,
            brain_id: None,
            hand_id: None,
            token_count: None,
        }
    }
}

//! Test-only aggregation helpers for workspace tool statistics.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use moa_core::{Event, EventRecord, ToolCallId};

use super::{ToolStats, WorkspaceToolStats, update_ema};

pub(super) const TOOL_STATS_EMA_ALPHA: f64 = 0.1;
const MAX_COMMON_ERRORS: usize = 3;

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

pub(super) fn workspace_tool_stats_from_events(
    events: &[EventRecord],
) -> Option<WorkspaceToolStats> {
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

fn top_error_patterns(patterns: HashMap<String, u32>) -> Vec<(String, u32)> {
    let mut entries = patterns.into_iter().collect::<Vec<_>>();
    entries.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    entries.truncate(MAX_COMMON_ERRORS);
    entries
}

fn record_error_pattern(errors: &mut HashMap<String, u32>, raw: &str) {
    let normalized = normalize_error_pattern(raw);
    if normalized.is_empty() {
        return;
    }
    *errors.entry(normalized).or_insert(0) += 1;
}

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

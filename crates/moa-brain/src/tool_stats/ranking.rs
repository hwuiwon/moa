//! Test-only tool schema ranking and annotation helpers.

use std::cmp::Ordering;

use serde_json::Value;

use super::{ToolStats, WorkspaceToolStats};

const TOOL_RANKING_MIN_CALLS: u64 = 5;
const TOOL_ANNOTATION_MIN_CALLS: u64 = 10;
const TOOL_WARNING_SUCCESS_THRESHOLD: f64 = 0.8;

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

fn compare_within_tier(left: Option<&ToolStats>, right: Option<&ToolStats>, tier: u8) -> Ordering {
    match tier {
        0 => compare_success_first(left, right),
        2 => compare_failure_last(left, right),
        _ => Ordering::Equal,
    }
}

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

fn compare_f64_desc(left: f64, right: f64) -> Ordering {
    right.partial_cmp(&left).unwrap_or(Ordering::Equal)
}

fn compare_f64_asc(left: f64, right: f64) -> Ordering {
    left.partial_cmp(&right).unwrap_or(Ordering::Equal)
}

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

pub(super) fn tool_annotation(stats: &ToolStats) -> Option<String> {
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

fn format_percentage(value: f64) -> String {
    format!("{:.0}%", (value.clamp(0.0, 1.0) * 100.0).round())
}

fn format_duration(duration_ms: f64) -> String {
    if duration_ms >= 1000.0 {
        format!("{:.1}s", duration_ms / 1000.0)
    } else {
        format!("{duration_ms:.0}ms")
    }
}

fn failure_rate(stats: &ToolStats) -> f64 {
    if stats.total_calls == 0 {
        0.0
    } else {
        stats.failures as f64 / stats.total_calls as f64
    }
}

fn schema_name(schema: &Value) -> Option<&str> {
    schema.get("name").and_then(Value::as_str)
}

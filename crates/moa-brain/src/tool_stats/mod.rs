//! Workspace-scoped tool performance tracking.

#[cfg(test)]
mod aggregation;
#[cfg(test)]
mod ranking;
#[cfg(test)]
mod tests;

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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

/// Updates an exponential moving average from one new observation.
pub fn update_ema(current: f64, observation: f64, alpha: f64) -> f64 {
    alpha * observation + (1.0 - alpha) * current
}

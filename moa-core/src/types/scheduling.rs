//! Scheduling DTOs shared across orchestrator implementations.

use serde::{Deserialize, Serialize};

/// Cron specification for scheduled background jobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronSpec {
    /// Human-readable job name.
    pub name: String,
    /// Cron schedule expression.
    pub schedule: String,
    /// Task identifier or type.
    pub task: String,
}

/// Handle returned for a registered cron job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CronHandle {
    /// Local scheduler handle.
    Local { id: String },
}

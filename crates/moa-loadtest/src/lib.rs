//! Custom load-test harness for realistic MOA multi-turn agent workloads.

pub mod scenarios;

mod backend;
mod config;
mod harness;
mod metrics;
mod options;
mod plan;
mod report;
mod runner;

#[cfg(test)]
mod tests;

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use clap::ValueEnum;
use moa_core::{
    Event, EventRecord, MoaConfig, MoaError, ModelId, ModelTask, Platform, Result, SessionId,
    SessionMeta, SessionStatus, UserId, WorkspaceId,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use uuid::Uuid;

pub use harness::run_loadtest;
pub use options::{LoadMode, LoadTestOptions, OutputFormat, SessionProfileKind};
pub use report::{
    LoadTestReport, PercentileSummary, SessionReport, render_human_report, render_json_report,
};

pub(crate) use backend::*;
pub(crate) use config::*;
pub(crate) use metrics::*;
pub(crate) use plan::*;
pub(crate) use runner::*;

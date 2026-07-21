//! Custom load-test harness for realistic MOA multi-turn agent workloads.

pub mod scenarios;

mod backend;
mod edge_backend;
mod harness;
mod hist;
mod merge;
mod metrics;
mod options;
mod plan;
mod report;
mod runner;
mod schedule;
mod tenancy;

#[cfg(test)]
mod tests;

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use clap::ValueEnum;
use moa_config::MoaConfig;
use moa_core::{
    error::MoaError, error::Result, events::Event, types::channel::Channel,
    types::contact::SessionActorRef, types::events_stream::EventRecord,
    types::identifiers::ModelId, types::identifiers::SessionId, types::identifiers::TenantId,
    types::provider::ModelTask, types::session::SessionMeta, types::session::SessionStatus,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use uuid::Uuid;

pub use harness::run_loadtest;
pub use hist::SerializedHistograms;
pub use merge::{MergedSummary, merge_report_files, render_merged_summary};
pub use options::{LoadMode, LoadTestOptions, OutputFormat, SessionProfileKind};
pub use report::{
    ErrorTaxonomy, EventAppendPhaseLatencyReport, EventAppendTypeReport, LoadTestReport,
    PercentileSummary, ResourceBillReport, SessionReport, StepLatencyReport, WindowReport,
    render_human_report, render_json_report,
};
pub use schedule::{ArrivalProcess, LoadShape};

pub(crate) use backend::*;
pub(crate) use edge_backend::*;
pub(crate) use hist::*;
pub(crate) use metrics::*;
pub(crate) use plan::*;
pub(crate) use runner::*;
pub(crate) use schedule::*;
pub(crate) use tenancy::*;

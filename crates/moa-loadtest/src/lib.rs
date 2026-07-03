//! Custom load-test harness for realistic MOA multi-turn agent workloads.

pub mod scenarios;

mod backend;
mod config;
mod edge_backend;
mod harness;
mod hist;
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
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use clap::ValueEnum;
use moa_core::{
    Channel, Event, EventRecord, MoaConfig, MoaError, ModelId, ModelTask, Result, SessionActorRef,
    SessionId, SessionMeta, SessionStatus, TenantId,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use uuid::Uuid;

pub use harness::run_loadtest;
pub use options::{LoadMode, LoadTestOptions, OutputFormat, SessionProfileKind};
pub use report::{
    ErrorTaxonomy, LoadTestReport, PercentileSummary, SessionReport, StepLatencyReport,
    WindowReport, render_human_report, render_json_report,
};
pub use schedule::ArrivalProcess;

pub(crate) use backend::*;
pub(crate) use config::*;
pub(crate) use edge_backend::*;
pub(crate) use hist::*;
pub(crate) use metrics::*;
pub(crate) use plan::*;
pub(crate) use runner::*;
pub(crate) use schedule::*;
pub(crate) use tenancy::*;

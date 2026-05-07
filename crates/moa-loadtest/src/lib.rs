//! Custom load-test harness for realistic MOA multi-turn agent workloads.

pub mod scenarios;

mod backend;
mod config;
mod daemon;
mod harness;
mod metrics;
mod options;
mod plan;
mod report;
mod runner;
mod scripted;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use clap::ValueEnum;
use moa_core::{
    ApprovalDecision, BrainOrchestrator, CompletionContent, CompletionRequest, CompletionResponse,
    CompletionStream, DaemonCommand, DaemonReply, DaemonStreamEvent, Event, EventRecord,
    LLMProvider, MoaConfig, MoaError, ModelCapabilities, ModelId, ModelTask, Platform, Result,
    RuntimeEvent, SessionId, SessionMeta, SessionSignal, SessionStatus, SessionStore,
    StartSessionRequest, TokenPricing, TokenUsage, ToolCallFormat, UserId, UserMessage,
    WorkspaceId,
};
use moa_hands::ToolRouter;
use moa_orchestrator_local::LocalOrchestrator;
use moa_providers::{ModelRouter, ScriptedResponse, resolve_provider_selection};
use moa_session::{
    PostgresSessionStore,
    testing::{cleanup_test_schema, test_database_url},
};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, broadcast, mpsc};
use uuid::Uuid;

pub use harness::run_loadtest;
pub use options::{LoadMode, LoadTarget, LoadTestOptions, OutputFormat, SessionProfileKind};
pub use report::{
    LoadTestReport, PercentileSummary, SessionReport, render_human_report, render_json_report,
};

pub(crate) use backend::*;
pub(crate) use config::*;
pub(crate) use daemon::*;
pub(crate) use metrics::*;
pub(crate) use plan::*;
pub(crate) use runner::*;
pub(crate) use scripted::*;

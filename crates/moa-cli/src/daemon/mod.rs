//! Local daemon server and client helpers for persistent background MOA operation.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use futures_util::FutureExt;
use moa_core::{
    BrainOrchestrator, BroadcastChannel, DaemonCommand, DaemonInfo, DaemonReply,
    DaemonSessionPreview, DaemonStreamEvent, EventRange, LagPolicy, MoaConfig, RecvResult,
    RuntimeEvent, SessionFilter, SessionId, SessionStatus, SessionStore, WorkspaceBudgetStatus,
    recv_with_lag_handling,
};
use moa_orchestrator_local::LocalOrchestrator;
use moa_session::PostgresSessionStore;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

use crate::api::start_api_server;

mod client;
mod paths;
mod protocol;
mod server;
#[cfg(test)]
mod tests;

pub use client::{daemon_info, daemon_logs, start_daemon, stop_daemon};
#[cfg(test)]
pub(super) use client::{request, wait_for_daemon};
use client::{write_reply, write_stream_event};
use paths::{
    daemon_log_path, daemon_pid_path, daemon_socket_path, ensure_parent_dir, read_pid_file,
};
use protocol::handle_connection;
pub use server::run_daemon_server;

/// Shared daemon server state.
#[derive(Clone)]
pub(super) struct DaemonState {
    pub(super) orchestrator: Arc<LocalOrchestrator>,
    pub(super) session_store: Arc<PostgresSessionStore>,
    pub(super) info: Arc<DaemonInfo>,
    pub(super) daily_workspace_budget_cents: u32,
}

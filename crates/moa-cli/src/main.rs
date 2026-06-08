//! CLI entry point for MOA subcommands and orchestrator diagnostics.

mod analytics;
mod checkpoint;
mod cli;
mod client;
mod commands;
mod dispatch;
mod doctor;
mod eval;
mod exec;
mod init;
mod lineage;
mod memory;
mod orchestrator;
mod support;
#[cfg(test)]
mod tests;
mod version;

use std::env;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ::moa_memory_graph as memory_graph;
use ::moa_memory_ingest as memory_ingest;
use ::moa_memory_vector as memory_vector;
use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::{Args, CommandFactory, Parser, Subcommand};
use memory_graph::{AgeGraphStore, GraphStore, PiiClass};
use memory_ingest::{IngestApplyReport, SessionTurn};
use memory_vector::PgvectorStore;
use moa_brain::retrieval::{HybridRetriever, RetrievalRequest};
use moa_core::{
    BranchManager, LineageHandle, MemoryScope, MoaConfig, OtlpProtocol, ScopeContext,
    SessionFilter, SessionId, SessionStatus, TelemetryConfig, UserId, WorkspaceId,
    default_log_path, init_observability, metrics_endpoint_url,
};
use moa_eval::{
    AgentConfig, EngineOptions, EvalEngine, EvalRun, EvalStatus, EvaluatorOptions, ReporterOptions,
    build_evaluators, build_reporters, discover_suites, evaluate_run, list_datasets,
    load_agent_config, load_suite, register_dataset, replay_dataset_live,
};
use moa_lineage_audit::{DsarExporter, HashChain, SigningKey, blake3_merkle_root, hash_from_slice};
use moa_lineage_core::{
    BackendIntrospection, FusedHit, LineageEvent, RerankHit, RetrievalLineage, RetrievalStage,
    StageTimings, TurnId, VecHit,
};
use moa_lineage_sink::{MpscSink, MpscSinkConfig};
use moa_session::{NeonBranchManager, PostgresSessionStore, create_session_store};
use sqlx::Row;
use tokio::fs;
use tokio::process::Command;
use tokio::time::timeout;
use uuid::Uuid;

use commands::admin::{
    AdminCommand, PromoteWorkspaceArgs, WorkspacePromotionArgs, handle_admin_command,
};
use commands::agents::{AgentsCommand, handle_agents_command};
use commands::approvals::{ApprovalsCommand, handle_approvals_command};
use commands::audit::{AuditCommand, handle_audit_command};
use commands::auth::{AuthCommand, handle_auth_command};
use commands::authz::{AuthzCommand, handle_authz_command};
use commands::privacy::{PrivacyCommand, handle_privacy_command};
use commands::skills::{SkillsCommand, handle_skills_command};
use commands::tenants::{TenantsCommand, handle_tenants_command};

pub(crate) use analytics::*;
pub(crate) use checkpoint::*;
pub(crate) use cli::*;
pub(crate) use doctor::*;
pub(crate) use eval::*;
pub(crate) use init::*;
pub(crate) use lineage::*;
pub(crate) use memory::*;
pub(crate) use support::*;
pub(crate) use version::*;

/// Runs the `moa` CLI binary.
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = MoaConfig::load()?;
    let _telemetry = init_observability(
        &config,
        &TelemetryConfig {
            debug: cli.debug,
            log_file: cli.log_file.clone(),
            json_stdout: false,
        },
    )?;

    dispatch::dispatch(cli, config).await
}

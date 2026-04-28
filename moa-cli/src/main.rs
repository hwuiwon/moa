//! CLI entry point for MOA subcommands and daemon management.

mod api;
mod daemon;
mod exec;

use std::env;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Args, CommandFactory, Parser, Subcommand};
use moa_core::{
    BranchManager, MemoryPath, MemoryScope, MemoryStore, MoaConfig, OtlpProtocol, SessionFilter,
    SessionStatus, SessionStore, TelemetryConfig, UserId, WorkspaceId, default_log_path,
    init_observability, metrics_endpoint_url,
};
use moa_eval::{
    AgentConfig, EngineOptions, EvalEngine, EvalRun, EvalStatus, EvaluatorOptions, ReporterOptions,
    build_evaluators, build_reporters, discover_suites, evaluate_run, load_agent_config,
    load_suite,
};
use moa_memory::FileMemoryStore;
use moa_session::{NeonBranchManager, PostgresSessionStore, create_session_store};
use moa_skills::run_skill_suite;
use tokio::fs;
use tokio::process::Command;
use tokio::time::timeout;
use uuid::Uuid;
/// Top-level MOA command line interface.
#[derive(Debug, Parser)]
#[command(name = "moa", about = "MOA local terminal agent", version)]
struct Cli {
    /// Enable debug logging to a file instead of the terminal.
    #[arg(long)]
    debug: bool,

    /// Override the debug log file path.
    #[arg(long, value_name = "PATH")]
    log_file: Option<PathBuf>,

    /// Runs one prompt and prints the final assistant response when no subcommand is supplied.
    #[arg(value_name = "PROMPT")]
    prompt: Option<String>,

    #[command(subcommand)]
    command: Option<CommandKind>,
}

/// Supported CLI subcommands.
#[derive(Debug, Subcommand)]
enum CommandKind {
    /// Runs one prompt and prints the final assistant response to stdout.
    Exec(ExecArgs),
    /// Shows active daemon/session status.
    Status,
    /// Lists persisted sessions.
    Sessions(SessionsArgs),
    /// Session-specific analytics.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Workspace-scoped analytics.
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
    /// Tool analytics.
    Tool {
        #[command(subcommand)]
        command: ToolCommand,
    },
    /// Cache analytics.
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
    /// Memory-related CLI operations.
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },
    /// Reads or updates config values.
    Config {
        #[command(subcommand)]
        command: Option<ConfigCommand>,
    },
    /// Initializes MOA directories for the current workspace.
    Init,
    /// Prints version information.
    Version,
    /// Prints a local environment diagnostic report.
    Doctor,
    /// Controls the background daemon.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Manages Neon checkpoint branches.
    Checkpoint {
        #[command(subcommand)]
        command: CheckpointCommand,
    },
    /// Runs agent evaluation suites.
    Eval {
        #[command(subcommand)]
        command: EvalCommand,
    },
}

/// One-shot exec arguments.
#[derive(Debug, Args)]
struct ExecArgs {
    /// Prompt text to submit.
    #[arg(required = true)]
    prompt: String,
}

/// Session-list filtering arguments.
#[derive(Debug, Args)]
struct SessionsArgs {
    /// Restrict sessions to one workspace id or `.` for the current directory.
    #[arg(long)]
    workspace: Option<String>,
}

/// Session analytics commands.
#[derive(Debug, Subcommand)]
enum SessionCommand {
    /// Shows summary stats for one session.
    Stats {
        /// Session id to inspect.
        id: String,
    },
}

/// Workspace analytics commands.
#[derive(Debug, Subcommand)]
enum WorkspaceCommand {
    /// Shows workspace rollups over a recent window.
    Stats(WorkspaceStatsArgs),
}

/// Tool analytics commands.
#[derive(Debug, Subcommand)]
enum ToolCommand {
    /// Shows per-tool latency and success metrics.
    Stats(ToolStatsArgs),
}

/// Cache analytics commands.
#[derive(Debug, Subcommand)]
enum CacheCommand {
    /// Shows cache usage trends for a workspace.
    Stats(CacheStatsArgs),
}

/// Arguments for `moa workspace stats`.
#[derive(Debug, Args)]
struct WorkspaceStatsArgs {
    /// Workspace id override. Use `.` for the current directory workspace.
    #[arg(long)]
    workspace: Option<String>,

    /// Number of days to include.
    #[arg(long, default_value_t = 30)]
    days: u32,
}

/// Arguments for `moa tool stats`.
#[derive(Debug, Args)]
struct ToolStatsArgs {
    /// Optional workspace filter. Use `.` for the current directory workspace.
    #[arg(long)]
    workspace: Option<String>,
}

/// Arguments for `moa cache stats`.
#[derive(Debug, Args)]
struct CacheStatsArgs {
    /// Workspace id override. Use `.` for the current directory workspace.
    #[arg(long)]
    workspace: Option<String>,

    /// Number of days to include.
    #[arg(long, default_value_t = 30)]
    days: u32,
}

/// Memory CLI commands.
#[derive(Debug, Subcommand)]
enum MemoryCommand {
    /// Searches workspace memory.
    Search {
        /// Search query.
        query: String,
    },
    /// Displays one memory page.
    Show {
        /// Logical memory path.
        path: String,
    },
    /// Ingests one or more documents into workspace memory.
    Ingest(IngestArgs),
    /// Rebuilds the derived Postgres memory index from markdown files on disk.
    RebuildIndex(RebuildIndexArgs),
    /// Reports that legacy wiki embedding backfill is disabled.
    RebuildEmbeddings(RebuildEmbeddingsArgs),
}

/// Arguments for `moa memory ingest`.
#[derive(Debug, Args)]
struct IngestArgs {
    /// File path(s) to ingest. Shell expansion can be used for batches.
    #[arg(required = true)]
    files: Vec<PathBuf>,

    /// Optional source name override for a single file.
    #[arg(long)]
    name: Option<String>,

    /// Workspace id override. Use `.` for the current directory workspace.
    #[arg(long)]
    workspace: Option<String>,
}

/// Arguments for `moa memory rebuild-index`.
#[derive(Debug, Args)]
struct RebuildIndexArgs {
    /// Rebuild every discovered scope under the local memory root.
    #[arg(long)]
    all: bool,

    /// Workspace id override. Use `.` for the current directory workspace.
    #[arg(long)]
    workspace: Option<String>,

    /// Rebuild the local user-memory scope instead of a workspace scope.
    #[arg(long)]
    user: bool,
}

/// Arguments for `moa memory rebuild-embeddings`.
#[derive(Debug, Args)]
struct RebuildEmbeddingsArgs {
    /// Re-enqueue every discovered scope under the local memory root.
    #[arg(long)]
    all: bool,

    /// Workspace id override. Use `.` for the current directory workspace.
    #[arg(long)]
    workspace: Option<String>,

    /// Re-enqueue the local user-memory scope instead of a workspace scope.
    #[arg(long)]
    user: bool,
}

/// Config CLI commands.
#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Updates a supported config key.
    Set {
        /// Dotted config key name.
        key: String,
        /// New value.
        value: String,
    },
}

/// Daemon CLI commands.
#[derive(Debug, Subcommand)]
enum DaemonCommand {
    /// Starts the background daemon.
    Start,
    /// Stops the background daemon.
    Stop,
    /// Shows daemon status.
    Status,
    /// Prints the daemon log tail.
    Logs,
    /// Runs the daemon server in the foreground.
    #[command(hide = true)]
    Serve,
}

/// Checkpoint CLI commands.
#[derive(Debug, Subcommand)]
enum CheckpointCommand {
    /// Creates a named checkpoint branch.
    Create {
        /// Human-readable checkpoint label.
        label: String,
    },
    /// Lists active MOA checkpoint branches.
    List,
    /// Switches the configured database URL to a checkpoint branch.
    Rollback {
        /// Neon checkpoint branch identifier.
        id: String,
    },
    /// Deletes expired checkpoint branches.
    Cleanup,
}

/// Eval CLI commands.
#[derive(Debug, Subcommand)]
enum EvalCommand {
    /// Runs a suite against one or more agent configs.
    Run(EvalRunArgs),
    /// Shows the eval plan without executing.
    Plan(EvalPlanArgs),
    /// Runs the regression suite for one workspace skill.
    Skill(EvalSkillArgs),
    /// Lists discoverable eval suites in a directory.
    List {
        /// Directory to scan for suites.
        #[arg(default_value = "tests/suites")]
        dir: PathBuf,
    },
}

/// Arguments for `moa eval run`.
#[derive(Debug, Args)]
struct EvalRunArgs {
    /// Path to the test suite file.
    #[arg(long)]
    suite: PathBuf,

    /// Paths to one or more agent config files.
    #[arg(long, required = true)]
    config: Vec<PathBuf>,

    /// Report sink spec: `terminal`, `json:<path>`, or `langfuse`.
    #[arg(long, default_value = "terminal")]
    report: Vec<String>,

    /// Maximum concurrent eval executions.
    #[arg(long, default_value_t = 1)]
    parallel: usize,

    /// Exit non-zero when any run fails, errors, or times out.
    #[arg(long)]
    ci: bool,

    /// Evaluators to run.
    #[arg(
        long,
        default_values_t = vec![
            String::from("trajectory"),
            String::from("output"),
            String::from("tool_success")
        ]
    )]
    evaluator: Vec<String>,

    /// Maximum allowed per-run cost in dollars.
    #[arg(long)]
    max_cost: Option<f64>,

    /// Maximum allowed per-run latency in milliseconds.
    #[arg(long)]
    max_latency: Option<u64>,

    /// Maximum allowed tokens per run.
    #[arg(long)]
    max_tokens: Option<usize>,

    /// Maximum allowed tool calls per run.
    #[arg(long)]
    max_tool_calls: Option<usize>,

    /// Maximum allowed turns per run.
    #[arg(long)]
    max_turns: Option<usize>,

    /// Include per-case response and score comments in terminal output.
    #[arg(long, short)]
    verbose: bool,
}

/// Arguments for `moa eval skill`.
#[derive(Debug, Args)]
struct EvalSkillArgs {
    /// Skill name, path fragment, or full memory path.
    skill: String,

    /// Report sink spec: `terminal`, `json:<path>`, or `langfuse`.
    #[arg(long, default_value = "terminal")]
    report: Vec<String>,

    /// Verbose output with per-case detail.
    #[arg(long, short)]
    verbose: bool,

    /// Exit non-zero when the skill suite fails.
    #[arg(long)]
    ci: bool,
}

/// Arguments for `moa eval plan`.
#[derive(Debug, Args)]
struct EvalPlanArgs {
    /// Path to the test suite file.
    #[arg(long)]
    suite: PathBuf,

    /// Paths to one or more agent config files.
    #[arg(long, required = true)]
    config: Vec<PathBuf>,
}

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

    match cli.command {
        None => {
            if let Some(prompt) = cli.prompt {
                exec::run_exec(config, prompt).await?;
            } else {
                let mut command = Cli::command();
                command.print_long_help()?;
                println!();
            }
        }
        Some(CommandKind::Exec(args)) => {
            exec::run_exec(config, args.prompt).await?;
        }
        Some(CommandKind::Status) => {
            print!("{}", status_report(&config).await?);
        }
        Some(CommandKind::Sessions(args)) => {
            print!(
                "{}",
                sessions_report(&config, args.workspace.as_deref()).await?
            );
        }
        Some(CommandKind::Session { command }) => match command {
            SessionCommand::Stats { id } => {
                print!("{}", session_stats_report(&config, &id).await?);
            }
        },
        Some(CommandKind::Workspace { command }) => match command {
            WorkspaceCommand::Stats(args) => {
                print!(
                    "{}",
                    workspace_stats_report(&config, args.workspace.as_deref(), args.days).await?
                );
            }
        },
        Some(CommandKind::Tool { command }) => match command {
            ToolCommand::Stats(args) => {
                print!(
                    "{}",
                    tool_stats_report(&config, args.workspace.as_deref()).await?
                );
            }
        },
        Some(CommandKind::Cache { command }) => match command {
            CacheCommand::Stats(args) => {
                print!(
                    "{}",
                    cache_stats_report(&config, args.workspace.as_deref(), args.days).await?
                );
            }
        },
        Some(CommandKind::Memory { command }) => match command {
            MemoryCommand::Search { query } => {
                print!("{}", memory_search_report(&config, &query).await?);
            }
            MemoryCommand::Show { path } => {
                print!("{}", memory_show_report(&config, &path).await?);
            }
            MemoryCommand::Ingest(args) => {
                print!(
                    "{}",
                    memory_ingest_report(
                        &config,
                        &args.files,
                        args.name.as_deref(),
                        args.workspace.as_deref(),
                    )
                    .await?
                );
            }
            MemoryCommand::RebuildIndex(args) => {
                print!(
                    "{}",
                    memory_rebuild_index_report(
                        &config,
                        args.all,
                        args.workspace.as_deref(),
                        args.user,
                    )
                    .await?
                );
            }
            MemoryCommand::RebuildEmbeddings(args) => {
                print!(
                    "{}",
                    memory_rebuild_embeddings_report(
                        &config,
                        args.all,
                        args.workspace.as_deref(),
                        args.user,
                    )
                    .await?
                );
            }
        },
        Some(CommandKind::Config { command }) => match command {
            None => {
                let rendered = toml::to_string_pretty(&config).context("serializing config")?;
                print!("{rendered}");
            }
            Some(ConfigCommand::Set { key, value }) => {
                let mut updated = config;
                apply_config_update(&mut updated, &key, &value)?;
                updated.save_async().await?;
                print!("{}", toml::to_string_pretty(&updated)?);
            }
        },
        Some(CommandKind::Init) => {
            init_workspace(&config).await?;
            println!("initialized MOA workspace for {}", current_workspace_id());
        }
        Some(CommandKind::Version) => {
            println!("{}", version_text());
        }
        Some(CommandKind::Doctor) => {
            let log_path = cli.log_file.clone().unwrap_or_else(default_log_path);
            print!("{}", doctor_report(&config, &log_path).await?);
        }
        Some(CommandKind::Daemon { command }) => match command {
            DaemonCommand::Start => daemon::start_daemon(&config).await?,
            DaemonCommand::Stop => daemon::stop_daemon(&config).await?,
            DaemonCommand::Status => print!("{}", daemon_status_report(&config).await?),
            DaemonCommand::Logs => print!("{}", daemon::daemon_logs(&config).await?),
            DaemonCommand::Serve => daemon::run_daemon_server(config).await?,
        },
        Some(CommandKind::Checkpoint { command }) => match command {
            CheckpointCommand::Create { label } => {
                print!("{}", checkpoint_create_report(&config, &label).await?);
            }
            CheckpointCommand::List => {
                print!("{}", checkpoint_list_report(&config).await?);
            }
            CheckpointCommand::Rollback { id } => {
                print!("{}", checkpoint_rollback_report(config, &id).await?);
            }
            CheckpointCommand::Cleanup => {
                print!("{}", checkpoint_cleanup_report(&config).await?);
            }
        },
        Some(CommandKind::Eval { command }) => match command {
            EvalCommand::Run(args) => {
                let exit_code = handle_eval_run(args, config).await?;
                if exit_code != 0 {
                    std::process::exit(exit_code);
                }
            }
            EvalCommand::Plan(args) => {
                handle_eval_plan(args, config)?;
            }
            EvalCommand::Skill(args) => {
                let exit_code = handle_eval_skill(args, config).await?;
                if exit_code != 0 {
                    std::process::exit(exit_code);
                }
            }
            EvalCommand::List { dir } => {
                handle_eval_list(dir)?;
            }
        },
    }

    Ok(())
}

/// Returns a plain-text version string.
fn version_text() -> String {
    format!("moa {}", env!("CARGO_PKG_VERSION"))
}

async fn handle_eval_run(args: EvalRunArgs, config: MoaConfig) -> Result<i32> {
    let suite = load_suite(&args.suite).context("loading eval suite")?;
    let configs = load_eval_configs(&args.config)?;
    let evaluators = build_evaluators(
        &args.evaluator,
        &EvaluatorOptions {
            max_cost_dollars: args.max_cost,
            max_latency_ms: args.max_latency,
            max_tokens: args.max_tokens,
            max_tool_calls: args.max_tool_calls,
            max_turns: args.max_turns,
        },
    )
    .context("building evaluators")?;
    let reporters = build_reporters(
        &args.report,
        &ReporterOptions {
            verbose: args.verbose,
            color: !args.ci && std::io::stdout().is_terminal(),
            json_pretty: true,
        },
    )
    .context("building reporters")?;

    let engine = EvalEngine::new(
        config,
        EngineOptions {
            parallel: args.parallel,
            ..EngineOptions::default()
        },
    )
    .context("creating eval engine")?;

    let mut run = engine
        .run_suite(&suite, &configs)
        .await
        .context("running eval suite")?;
    evaluate_run(&suite, &mut run, &evaluators)
        .await
        .context("scoring eval results")?;

    for reporter in &reporters {
        reporter
            .report(&suite, &configs, &run)
            .await
            .context("reporting eval results")?;
    }

    Ok(eval_exit_code(args.ci, &run))
}

fn handle_eval_plan(args: EvalPlanArgs, config: MoaConfig) -> Result<()> {
    let suite = load_suite(&args.suite).context("loading eval suite")?;
    let configs = load_eval_configs(&args.config)?;
    let engine =
        EvalEngine::new(config, EngineOptions::default()).context("creating eval engine")?;
    let plan = engine.plan(&suite, &configs);

    println!("Suite: {}", plan.suite_name);
    println!("Configs: {}", plan.configs.join(", "));
    println!("Cases: {}", plan.cases.join(", "));
    println!("Total runs: {}", plan.total_runs);
    println!(
        "Estimated cost: ${:.4} - ${:.4}",
        plan.estimated_cost_range.0, plan.estimated_cost_range.1
    );
    Ok(())
}

async fn handle_eval_skill(args: EvalSkillArgs, config: MoaConfig) -> Result<i32> {
    let memory_store = Arc::new(load_memory_store(&config).await?);
    let workspace_id = current_workspace_id();
    let skill_run = run_skill_suite(&config, memory_store, &workspace_id, &args.skill).await?;
    let reporters = build_reporters(
        &args.report,
        &ReporterOptions {
            verbose: args.verbose,
            color: !args.ci && std::io::stdout().is_terminal(),
            json_pretty: true,
        },
    )
    .context("building reporters")?;

    for reporter in &reporters {
        reporter
            .report(
                &skill_run.suite,
                std::slice::from_ref(&skill_run.config),
                &skill_run.run,
            )
            .await
            .context("reporting skill eval results")?;
    }

    Ok(eval_exit_code(args.ci, &skill_run.run))
}

fn handle_eval_list(dir: PathBuf) -> Result<()> {
    let paths = discover_suites(&dir).context("discovering eval suites")?;
    for path in paths {
        let suite =
            load_suite(&path).with_context(|| format!("loading suite from {}", path.display()))?;
        println!(
            "{:30} | {:3} cases | {}",
            suite.name,
            suite.cases.len(),
            suite.description.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

fn load_eval_configs(paths: &[PathBuf]) -> Result<Vec<AgentConfig>> {
    paths
        .iter()
        .map(|path| {
            load_agent_config(path)
                .with_context(|| format!("loading config from {}", path.display()))
        })
        .collect()
}

fn eval_exit_code(ci: bool, run: &EvalRun) -> i32 {
    if !ci {
        return 0;
    }
    if run
        .results
        .iter()
        .any(|result| matches!(result.status, EvalStatus::Error | EvalStatus::Timeout))
    {
        return 2;
    }
    if run
        .results
        .iter()
        .any(|result| matches!(result.status, EvalStatus::Failed))
    {
        return 1;
    }
    0
}

async fn status_report(config: &MoaConfig) -> Result<String> {
    let mut report = String::new();
    match daemon::daemon_info(config).await {
        Ok(info) => {
            report.push_str(&format!(
                "daemon: running\npid: {}\nsocket: {}\nsessions: {}\nactive_sessions: {}\n",
                info.pid, info.socket_path, info.session_count, info.active_session_count
            ));
        }
        Err(_) => report.push_str("daemon: stopped\n"),
    }

    let sessions = load_session_store(config)
        .await?
        .list_sessions(SessionFilter::default())
        .await?;
    let active = sessions
        .into_iter()
        .filter(|session| {
            matches!(
                session.status,
                SessionStatus::Created | SessionStatus::Running | SessionStatus::WaitingApproval
            )
        })
        .collect::<Vec<_>>();
    if active.is_empty() {
        report.push_str("active session table: none\n");
    } else {
        report.push_str("active session table:\n");
        for session in active {
            report.push_str(&format!(
                "- {} [{:?}] {} {}\n",
                session.session_id, session.status, session.workspace_id, session.model
            ));
        }
    }

    Ok(report)
}

async fn sessions_report(config: &MoaConfig, workspace: Option<&str>) -> Result<String> {
    let workspace_id = workspace.map(resolve_workspace_arg);
    let sessions = load_session_store(config)
        .await?
        .list_sessions(SessionFilter {
            workspace_id,
            ..SessionFilter::default()
        })
        .await?;
    let mut report = String::new();
    for session in sessions {
        report.push_str(&format!(
            "{}\t{:?}\t{}\t{}\n",
            session.session_id, session.status, session.workspace_id, session.model
        ));
    }
    Ok(report)
}

async fn session_stats_report(config: &MoaConfig, id: &str) -> Result<String> {
    let session_id = moa_core::SessionId(
        Uuid::parse_str(id).with_context(|| format!("invalid session id `{id}`"))?,
    );
    let store = load_session_store(config).await?;
    let summary = store.get_session_summary(session_id).await?;

    Ok(format!(
        "session: {}\nworkspace: {}\nuser: {}\nstatus: {:?}\nturns: {}\nevents: {}\ntools: {}\nerrors: {}\nduration_seconds: {:.3}\ntokens: in {} · out {}\ncost: {}\ncache_hit_rate: {:.2}%\n",
        summary.session_id,
        summary.workspace_id,
        summary.user_id,
        summary.status,
        summary.turn_count,
        summary.event_count,
        summary.tool_call_count,
        summary.error_count,
        summary.duration_seconds,
        summary.total_input_tokens,
        summary.total_output_tokens,
        format_cents(summary.total_cost_cents),
        summary.cache_hit_rate * 100.0
    ))
}

async fn workspace_stats_report(
    config: &MoaConfig,
    workspace: Option<&str>,
    days: u32,
) -> Result<String> {
    let workspace_id = workspace
        .map(resolve_workspace_arg)
        .unwrap_or_else(current_workspace_id);
    let store = load_session_store(config).await?;
    store.refresh_analytics_materialized_views().await?;
    let summary = store.get_workspace_stats(&workspace_id, days).await?;

    Ok(format!(
        "workspace: {}\nwindow_days: {}\nsessions: {}\nturns: {}\ntokens: in {} · cache_read {} · out {}\ncost: {}\ncache_hit_rate: {:.2}%\n",
        summary.workspace_id,
        summary.days,
        summary.session_count,
        summary.turn_count,
        summary.total_input_tokens,
        summary.total_cache_read_tokens,
        summary.total_output_tokens,
        format_cents(summary.total_cost_cents),
        summary.cache_hit_rate * 100.0
    ))
}

async fn tool_stats_report(config: &MoaConfig, workspace: Option<&str>) -> Result<String> {
    let workspace_id = workspace.map(resolve_workspace_arg);
    let store = load_session_store(config).await?;
    let rows = store
        .list_tool_call_summaries(workspace_id.as_ref())
        .await?;

    let mut report = String::new();
    if let Some(workspace_id) = workspace_id {
        report.push_str(&format!("workspace: {}\n", workspace_id));
    }
    if rows.is_empty() {
        report.push_str("tool stats: none\n");
        return Ok(report);
    }

    report.push_str("tool\tcalls\tsuccess\tavg_ms\tp50_ms\tp95_ms\n");
    for row in rows {
        report.push_str(&format!(
            "{}\t{}\t{:.2}%\t{:.2}\t{:.2}\t{:.2}\n",
            row.tool_name,
            row.call_count,
            row.success_rate * 100.0,
            row.avg_duration_ms,
            row.p50_ms,
            row.p95_ms
        ));
    }
    Ok(report)
}

async fn cache_stats_report(
    config: &MoaConfig,
    workspace: Option<&str>,
    days: u32,
) -> Result<String> {
    let workspace_id = workspace
        .map(resolve_workspace_arg)
        .unwrap_or_else(current_workspace_id);
    let store = load_session_store(config).await?;
    store.refresh_analytics_materialized_views().await?;
    let summary = store.get_workspace_stats(&workspace_id, days).await?;
    let daily = store.list_cache_daily_metrics(&workspace_id, days).await?;

    let mut report = format!(
        "workspace: {}\nwindow_days: {}\ncache_hit_rate: {:.2}%\ncached_input_tokens: {}\ntotal_input_tokens: {}\nestimated_savings: unavailable (pricing history is not normalized in SQL yet)\n",
        summary.workspace_id,
        summary.days,
        summary.cache_hit_rate * 100.0,
        summary.total_cache_read_tokens,
        summary.total_input_tokens
    );
    if daily.is_empty() {
        report.push_str("daily: none\n");
        return Ok(report);
    }

    report.push_str("day\tcache_hit_rate\tcached_input_tokens\ttotal_input_tokens\tcost\n");
    for row in daily {
        report.push_str(&format!(
            "{}\t{:.2}%\t{}\t{}\t{}\n",
            row.day.format("%Y-%m-%d"),
            row.avg_cache_hit_rate * 100.0,
            row.total_cache_read_tokens,
            row.total_input_tokens,
            format_cents(row.total_cost_cents)
        ));
    }
    Ok(report)
}

async fn memory_search_report(config: &MoaConfig, query: &str) -> Result<String> {
    let store = load_memory_store(config).await?;
    let results = store
        .search(
            query,
            &MemoryScope::Workspace {
                workspace_id: current_workspace_id(),
            },
            20,
        )
        .await?;
    let mut report = String::new();
    for result in results {
        report.push_str(&format!(
            "{}\t{}\t{}\n",
            result.path, result.title, result.snippet
        ));
    }
    Ok(report)
}

async fn memory_show_report(config: &MoaConfig, path: &str) -> Result<String> {
    let store = load_memory_store(config).await?;
    let path = MemoryPath::new(path);
    let page = store
        .read_page(
            &MemoryScope::Workspace {
                workspace_id: current_workspace_id(),
            },
            &path,
        )
        .await?;
    let rendered = toml::to_string(&page.metadata).unwrap_or_default();
    Ok(format!("---\n{}---\n{}", rendered, page.content))
}

async fn memory_ingest_report(
    config: &MoaConfig,
    files: &[PathBuf],
    name: Option<&str>,
    workspace: Option<&str>,
) -> Result<String> {
    if files.is_empty() {
        bail!("at least one file path is required");
    }
    if files.len() > 1 && name.is_some() {
        bail!("--name can only be used when ingesting a single file");
    }

    let store = load_memory_store(config).await?;
    let scope = MemoryScope::Workspace {
        workspace_id: workspace
            .map(resolve_workspace_arg)
            .unwrap_or_else(current_workspace_id),
    };

    let mut sections = Vec::with_capacity(files.len());
    for file in files {
        let content = fs::read_to_string(file)
            .await
            .with_context(|| format!("reading {}", file.display()))?;
        let source_name = match name {
            Some(value) => value.to_string(),
            None => derive_ingest_source_name(file),
        };
        let report = MemoryStore::ingest_source(&store, &scope, &source_name, &content).await?;
        sections.push(format_cli_ingest_section(file, &report));
    }

    let mut output = String::new();
    if files.len() > 1 {
        output.push_str(&format!(
            "Ingested {} documents into workspace memory.\n\n",
            files.len()
        ));
    }
    output.push_str(&sections.join("\n\n"));
    output.push('\n');
    Ok(output)
}

async fn doctor_report(config: &MoaConfig, log_path: &Path) -> Result<String> {
    let database_line = doctor_database(config).await;
    let mut lines = vec![
        "MOA doctor".to_string(),
        format!("provider: {}", config.general.default_provider),
        format!("model: {}", config.general.default_model),
        format!(
            "anthropic_key: {} ({})",
            env_presence(&config.providers.anthropic.api_key_env),
            config.providers.anthropic.api_key_env
        ),
        format!(
            "openai_key: {} ({})",
            env_presence(&config.providers.openai.api_key_env),
            config.providers.openai.api_key_env
        ),
        format!(
            "google_key: {} ({})",
            env_presence(&config.providers.google.api_key_env),
            config.providers.google.api_key_env
        ),
        format!("docker: {}", docker_status().await),
        format!("disk: {}", disk_status(config).await),
        format!("database: {database_line}"),
        format!("memory_index: {}", memory_index_status(config).await),
        format!(
            "memory_embeddings: {}",
            memory_embedding_status(config).await
        ),
        format!(
            "log_file: {}{}",
            log_path.display(),
            if cfg!(debug_assertions) || std::env::var_os("RUST_LOG").is_some() {
                " (set via --debug/--log-file or RUST_LOG)"
            } else {
                " (--debug to enable)"
            }
        ),
    ];
    lines.push(doctor_metrics(config).await);

    if let Ok(info) = daemon::daemon_info(config).await {
        lines.push(format!(
            "daemon: running (pid {}, active {})",
            info.pid, info.active_session_count
        ));
    } else {
        lines.push("daemon: stopped".to_string());
    }

    Ok(lines.join("\n") + "\n")
}

async fn doctor_metrics(config: &MoaConfig) -> String {
    if !config.metrics.enabled {
        return "Metrics endpoint: disabled".to_string();
    }

    let Some(url) = metrics_endpoint_url(&config.metrics) else {
        return format!(
            "Metrics endpoint: invalid listen address `{}`",
            config.metrics.listen
        );
    };

    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    else {
        return format!("Metrics endpoint: {url} - unavailable");
    };

    match client.get(&url).send().await {
        Ok(response) if response.status().is_success() => format!("Metrics endpoint: {url} - OK"),
        Ok(response) => {
            format!(
                "Metrics endpoint: {url} - HTTP {}",
                response.status().as_u16()
            )
        }
        Err(_) => format!("Metrics endpoint: {url} - unavailable"),
    }
}

async fn daemon_status_report(config: &MoaConfig) -> Result<String> {
    let info = daemon::daemon_info(config).await?;
    Ok(format!(
        "daemon: running\npid: {}\nsocket: {}\nlog: {}\nstarted_at: {}\nsessions: {}\nactive_sessions: {}\n",
        info.pid,
        info.socket_path,
        info.log_path,
        info.started_at,
        info.session_count,
        info.active_session_count
    ))
}

async fn init_workspace(config: &MoaConfig) -> Result<()> {
    let config_path = MoaConfig::default_path()?;
    if !config_path.exists() {
        config.save_async().await?;
    }
    let workspace_id = current_workspace_id();
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let workspace_memory = Path::new(&home)
        .join(".moa")
        .join("workspaces")
        .join(workspace_id.as_str())
        .join("memory");
    fs::create_dir_all(workspace_memory).await?;
    fs::create_dir_all(expand_tilde(&config.local.sandbox_dir)).await?;
    if config.cloud.enabled
        && let Some(memory_dir) = config.cloud.memory_dir.as_deref()
    {
        fs::create_dir_all(expand_tilde(memory_dir)).await?;
    }
    Ok(())
}

async fn load_session_store(config: &MoaConfig) -> Result<Arc<PostgresSessionStore>> {
    create_session_store(config)
        .await
        .context("opening session store")
}

async fn load_memory_store(config: &MoaConfig) -> Result<FileMemoryStore> {
    let session_store = load_session_store(config).await?;
    FileMemoryStore::from_config_with_pool(
        config,
        Arc::new(session_store.pool().clone()),
        session_store.schema_name(),
    )
    .await
    .context("opening memory store")
}

fn load_branch_manager(config: &MoaConfig) -> Result<NeonBranchManager> {
    NeonBranchManager::from_config(config).context("opening Neon branch manager")
}

fn resolve_workspace_arg(value: &str) -> WorkspaceId {
    if value == "." {
        return current_workspace_id();
    }

    WorkspaceId::new(value)
}

fn current_workspace_id() -> WorkspaceId {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let name = cwd
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("default");
    WorkspaceId::new(name)
}

fn current_user_id() -> moa_core::UserId {
    moa_core::UserId::new(
        env::var("USER")
            .or_else(|_| env::var("USERNAME"))
            .unwrap_or_else(|_| "local-user".to_string()),
    )
}

fn format_cents(cost_cents: u64) -> String {
    format!("${:.2}", cost_cents as f64 / 100.0)
}

fn derive_ingest_source_name(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("unnamed-source");
    stem.split(['-', '_', ' '])
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            let mut chars = segment.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_cli_ingest_section(path: &Path, report: &moa_core::IngestReport) -> String {
    let mut lines = vec![
        format!("Ingested \"{}\" ({})", report.source_name, path.display()),
        format!("Created: {}", report.source_path.as_str()),
        format!(
            "Updated: {} pages",
            report.affected_pages.len().saturating_sub(1)
        ),
    ];

    if !report.contradictions.is_empty() {
        lines.push(format!("Contradictions: {}", report.contradictions.len()));
    }

    lines.join("\n")
}

fn env_presence(key: &str) -> &'static str {
    if env::var(key).is_ok() {
        "present"
    } else {
        "missing"
    }
}

async fn docker_status() -> String {
    match timeout(
        std::time::Duration::from_secs(5),
        Command::new("docker").arg("info").output(),
    )
    .await
    {
        Err(_) => "unavailable (timed out)".to_string(),
        Ok(Ok(output)) if output.status.success() => "available".to_string(),
        Ok(Ok(output)) => format!("unhealthy (exit {})", output.status),
        Ok(Err(_)) => "missing".to_string(),
    }
}

async fn disk_status(config: &MoaConfig) -> String {
    let target = expand_tilde(&config.local.sandbox_dir);
    match Command::new("df").arg("-k").arg(&target).output().await {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            text.lines()
                .nth(1)
                .map(str::trim)
                .unwrap_or("available")
                .to_string()
        }
        Ok(output) => format!("unhealthy (exit {})", output.status),
        Err(error) => format!("unavailable ({error})"),
    }
}

async fn doctor_database(config: &MoaConfig) -> String {
    match load_session_store(config).await {
        Ok(store) => {
            let version = sqlx::query_scalar::<_, String>("SELECT version()")
                .fetch_one(store.pool())
                .await;
            let pgvector = sqlx::query_scalar::<_, String>(
                "SELECT extversion FROM pg_extension WHERE extname = 'vector'",
            )
            .fetch_optional(store.pool())
            .await;

            match (version, pgvector) {
                (Ok(version), Ok(pgvector)) => format!(
                    "{}; pgvector={}",
                    version.lines().next().unwrap_or("unknown"),
                    pgvector.unwrap_or_else(|| "NOT INSTALLED".to_string())
                ),
                (Err(error), _) => format!("unhealthy ({error})"),
                (_, Err(error)) => format!("pgvector check failed ({error})"),
            }
        }
        Err(error) => format!("unhealthy ({error})"),
    }
}

async fn memory_index_status(config: &MoaConfig) -> String {
    match load_memory_store(config).await {
        Ok(store) => match store
            .get_index(&MemoryScope::Workspace {
                workspace_id: current_workspace_id(),
            })
            .await
        {
            Ok(index) => format!("healthy ({} chars)", index.len()),
            Err(error) => format!("unhealthy ({error})"),
        },
        Err(error) => format!("unhealthy ({error})"),
    }
}

async fn memory_embedding_status(config: &MoaConfig) -> String {
    let _ = config;
    "healthy (legacy wiki embeddings disabled; use graph memory vectors)".to_string()
}

async fn memory_rebuild_index_report(
    config: &MoaConfig,
    rebuild_all: bool,
    workspace: Option<&str>,
    rebuild_user: bool,
) -> Result<String> {
    if rebuild_all && (workspace.is_some() || rebuild_user) {
        bail!("--all cannot be combined with --workspace or --user");
    }
    if rebuild_user && workspace.is_some() {
        bail!("--user cannot be combined with --workspace");
    }

    let store = load_memory_store(config).await?;
    let scopes = if rebuild_all {
        discover_memory_scopes(&store).await?
    } else if rebuild_user {
        vec![MemoryScope::User {
            workspace_id: workspace
                .map(resolve_workspace_arg)
                .unwrap_or_else(current_workspace_id),
            user_id: current_user_id(),
        }]
    } else {
        vec![MemoryScope::Workspace {
            workspace_id: workspace
                .map(resolve_workspace_arg)
                .unwrap_or_else(current_workspace_id),
        }]
    };

    let mut output = String::new();
    for scope in scopes {
        let pages = MemoryStore::list_pages(&store, &scope, None).await?;
        MemoryStore::rebuild_search_index(&store, &scope).await?;
        output.push_str(&format!("rebuilt {} pages in {:?}\n", pages.len(), scope));
    }
    Ok(output)
}

async fn memory_rebuild_embeddings_report(
    config: &MoaConfig,
    rebuild_all: bool,
    workspace: Option<&str>,
    rebuild_user: bool,
) -> Result<String> {
    let _ = (config, rebuild_all, workspace, rebuild_user);
    Ok("legacy wiki embeddings are disabled; graph memory vectors are maintained by moa-memory-vector\n".to_string())
}

async fn discover_memory_scopes(store: &FileMemoryStore) -> Result<Vec<MemoryScope>> {
    let mut scopes = Vec::new();

    let workspaces_root = store.base_dir().join("workspaces");
    if fs::try_exists(&workspaces_root).await? {
        let mut entries = fs::read_dir(&workspaces_root).await?;
        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_dir() {
                continue;
            }
            let workspace_id = WorkspaceId::new(entry.file_name().to_string_lossy().to_string());
            let memory_root = entry.path().join("memory");
            if fs::try_exists(&memory_root).await? {
                scopes.push(MemoryScope::Workspace {
                    workspace_id: workspace_id.clone(),
                });
            }

            let users_root = entry.path().join("users");
            if !fs::try_exists(&users_root).await? {
                continue;
            }
            let mut user_entries = fs::read_dir(&users_root).await?;
            while let Some(user_entry) = user_entries.next_entry().await? {
                if !user_entry.file_type().await?.is_dir() {
                    continue;
                }
                if !fs::try_exists(user_entry.path().join("memory")).await? {
                    continue;
                }
                scopes.push(MemoryScope::User {
                    workspace_id: workspace_id.clone(),
                    user_id: UserId::new(user_entry.file_name().to_string_lossy().to_string()),
                });
            }
        }
    }

    Ok(scopes)
}

fn apply_config_update(config: &mut MoaConfig, key: &str, value: &str) -> Result<()> {
    match key {
        "general.default_provider" => config.general.default_provider = value.to_string(),
        "general.default_model" => {
            config.general.default_model = value.to_string();
            config.models.main = value.to_string();
        }
        "models.main" => {
            config.models.main = value.to_string();
            config.general.default_model = value.to_string();
        }
        "models.auxiliary" => {
            config.models.auxiliary = (!value.trim().is_empty()).then(|| value.to_string());
        }
        "general.reasoning_effort" => config.general.reasoning_effort = value.to_string(),
        "cloud.enabled" => config.cloud.enabled = parse_bool(value)?,
        "cloud.memory_dir" => config.cloud.memory_dir = Some(value.to_string()),
        "local.docker_enabled" => config.local.docker_enabled = parse_bool(value)?,
        "local.sandbox_dir" => config.local.sandbox_dir = value.to_string(),
        "memory.embedding_provider" => config.memory.embedding_provider = value.to_string(),
        "memory.embedding_model" => config.memory.embedding_model = value.to_string(),
        "database.url" => config.database.url = value.to_string(),
        "database.admin_url" => config.database.admin_url = Some(value.to_string()),
        "database.max_connections" => {
            config.database.max_connections =
                value.parse().context("expected integer pool size")?;
        }
        "database.connect_timeout_seconds" => {
            config.database.connect_timeout_seconds =
                value.parse().context("expected integer timeout")?;
        }
        "database.neon.enabled" => config.database.neon.enabled = parse_bool(value)?,
        "database.neon.api_key_env" => config.database.neon.api_key_env = value.to_string(),
        "database.neon.project_id" => config.database.neon.project_id = value.to_string(),
        "database.neon.parent_branch_id" => {
            config.database.neon.parent_branch_id = value.to_string();
        }
        "database.neon.max_checkpoints" => {
            config.database.neon.max_checkpoints =
                value.parse().context("expected integer checkpoint count")?;
        }
        "database.neon.checkpoint_ttl_hours" => {
            config.database.neon.checkpoint_ttl_hours = value
                .parse()
                .context("expected integer checkpoint ttl hours")?;
        }
        "database.neon.pooled" => config.database.neon.pooled = parse_bool(value)?,
        "database.neon.suspend_timeout_seconds" => {
            config.database.neon.suspend_timeout_seconds =
                value.parse().context("expected integer suspend timeout")?;
        }
        "local.memory_dir" => config.local.memory_dir = value.to_string(),
        "daemon.auto_connect" => config.daemon.auto_connect = parse_bool(value)?,
        "daemon.socket_path" => config.daemon.socket_path = value.to_string(),
        "observability.enabled" => config.observability.enabled = parse_bool(value)?,
        "observability.service_name" => config.observability.service_name = value.to_string(),
        "observability.otlp_endpoint" => {
            config.observability.otlp_endpoint = Some(value.to_string());
        }
        "observability.otlp_protocol" => {
            config.observability.otlp_protocol = parse_otlp_protocol(value)?;
        }
        "observability.environment" => config.observability.environment = Some(value.to_string()),
        "observability.release" => config.observability.release = Some(value.to_string()),
        "observability.sample_rate" => {
            config.observability.sample_rate =
                value.parse().context("expected decimal sample rate")?;
        }
        "metrics.enabled" => config.metrics.enabled = parse_bool(value)?,
        "metrics.listen" => config.metrics.listen = value.to_string(),
        _ => bail!("unsupported config key: {key}"),
    }

    Ok(())
}
fn parse_otlp_protocol(value: &str) -> Result<OtlpProtocol> {
    match value.trim().to_ascii_lowercase().as_str() {
        "grpc" => Ok(OtlpProtocol::Grpc),
        "http" => Ok(OtlpProtocol::Http),
        _ => bail!("expected `grpc` or `http`, got {value}"),
    }
}

fn parse_bool(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => bail!("expected boolean value, got {value}"),
    }
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(relative) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return Path::new(&home).join(relative);
    }
    PathBuf::from(path)
}

async fn checkpoint_create_report(config: &MoaConfig, label: &str) -> Result<String> {
    let manager = load_branch_manager(config)?;
    let handle = manager
        .create_checkpoint(label, None)
        .await
        .context("creating Neon checkpoint")?;
    Ok(format!(
        "created checkpoint\nid: {}\nlabel: {}\ncreated_at: {}\nconnection_url: {}\n",
        handle.id, handle.label, handle.created_at, handle.connection_url
    ))
}

async fn checkpoint_list_report(config: &MoaConfig) -> Result<String> {
    let manager = load_branch_manager(config)?;
    let checkpoints = manager
        .list_checkpoints()
        .await
        .context("listing Neon checkpoints")?;
    if checkpoints.is_empty() {
        return Ok("no active checkpoints\n".to_string());
    }

    let mut lines = Vec::with_capacity(checkpoints.len() + 1);
    lines.push("active checkpoints:".to_string());
    for checkpoint in checkpoints {
        let age = format_checkpoint_age(checkpoint.handle.created_at);
        lines.push(format!(
            "- {}  {}  age={} parent={} size_bytes={}",
            checkpoint.handle.id,
            checkpoint.handle.label,
            age,
            checkpoint.parent_branch,
            checkpoint
                .size_bytes
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        ));
    }
    Ok(format!("{}\n", lines.join("\n")))
}

async fn checkpoint_rollback_report(mut config: MoaConfig, id: &str) -> Result<String> {
    let manager = load_branch_manager(&config)?;
    let checkpoint = manager
        .get_checkpoint(id)
        .await
        .context("loading checkpoint metadata")?
        .with_context(|| format!("checkpoint {id} not found"))?;
    manager
        .rollback_to(&checkpoint.handle)
        .await
        .context("preparing checkpoint rollback")?;
    config.database.url = checkpoint.handle.connection_url.clone();
    config.save_async().await.context("saving config")?;
    Ok(format!(
        "rolled back to checkpoint\nid: {}\nlabel: {}\ndatabase_url: {}\n",
        checkpoint.handle.id, checkpoint.handle.label, checkpoint.handle.connection_url
    ))
}

async fn checkpoint_cleanup_report(config: &MoaConfig) -> Result<String> {
    let manager = load_branch_manager(config)?;
    let deleted = manager
        .cleanup_expired()
        .await
        .context("cleaning up expired checkpoints")?;
    Ok(format!("deleted_expired_checkpoints: {deleted}\n"))
}

fn format_checkpoint_age(created_at: chrono::DateTime<chrono::Utc>) -> String {
    let age = chrono::Utc::now() - created_at;
    if age.num_hours() >= 1 {
        return format!("{}h", age.num_hours());
    }
    if age.num_minutes() >= 1 {
        return format!("{}m", age.num_minutes());
    }
    format!("{}s", age.num_seconds().max(0))
}

#[cfg(test)]
mod tests {
    use super::{
        apply_config_update, default_log_path, doctor_report, eval_exit_code, memory_ingest_report,
        parse_bool, version_text,
    };
    use moa_core::{MemoryPath, MemoryScope, MemoryStore, MoaConfig, WorkspaceId};
    use moa_eval::{EvalRun, EvalStatus, RunSummary};
    use moa_memory::FileMemoryStore;
    use tempfile::tempdir;
    use tokio::fs;

    #[test]
    fn version_command_uses_package_version() {
        assert_eq!(version_text(), format!("moa {}", env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn config_updates_known_keys() {
        let mut config = MoaConfig::default();
        apply_config_update(&mut config, "general.default_model", "claude-sonnet-4-6")
            .expect("update config");
        assert_eq!(config.general.default_model, "claude-sonnet-4-6");
        apply_config_update(&mut config, "database.max_connections", "5")
            .expect("update max connections");
        assert_eq!(config.database.max_connections, 5);
        apply_config_update(&mut config, "metrics.enabled", "true").expect("enable metrics");
        apply_config_update(&mut config, "metrics.listen", "127.0.0.1:19090")
            .expect("set metrics listen");
        assert!(config.metrics.enabled);
        assert_eq!(config.metrics.listen, "127.0.0.1:19090");
    }

    #[test]
    fn parse_bool_accepts_common_values() {
        assert!(parse_bool("yes").expect("bool"));
        assert!(!parse_bool("0").expect("bool"));
    }

    #[tokio::test]
    async fn doctor_report_includes_log_file_path() {
        let dir = tempdir().expect("temp dir");
        let base = dir.keep();
        let mut config = MoaConfig::default();
        config.local.memory_dir = base.join("memory").display().to_string();
        config.local.sandbox_dir = base.join("sandbox").display().to_string();
        config.daemon.socket_path = base.join("daemon.sock").display().to_string();
        config.daemon.pid_file = base.join("daemon.pid").display().to_string();
        config.daemon.log_file = base.join("daemon.log").display().to_string();
        config.daemon.auto_connect = false;

        let report = doctor_report(&config, &default_log_path())
            .await
            .expect("doctor report");
        assert!(report.contains("log_file: "));
        assert!(report.contains("Metrics endpoint: disabled"));
        assert!(
            report.contains("--debug to enable")
                || report.contains("set via --debug/--log-file or RUST_LOG")
        );
    }

    #[tokio::test]
    async fn doctor_report_uses_custom_log_file_path() {
        let dir = tempdir().expect("temp dir");
        let base = dir.keep();
        let mut config = MoaConfig::default();
        config.local.memory_dir = base.join("memory").display().to_string();
        config.local.sandbox_dir = base.join("sandbox").display().to_string();
        config.daemon.socket_path = base.join("daemon.sock").display().to_string();
        config.daemon.pid_file = base.join("daemon.pid").display().to_string();
        config.daemon.log_file = base.join("daemon.log").display().to_string();
        config.daemon.auto_connect = false;

        let custom_log = base.join("custom.log");
        let report = doctor_report(&config, &custom_log)
            .await
            .expect("doctor report");
        assert!(report.contains(&format!("log_file: {}", custom_log.display())));
        assert!(report.contains("Metrics endpoint: disabled"));
    }

    #[test]
    fn ci_exit_code_distinguishes_failures_and_errors() {
        let mut run = EvalRun {
            suite_name: "suite".to_string(),
            started_at: chrono::Utc::now(),
            completed_at: chrono::Utc::now(),
            results: Vec::new(),
            summary: RunSummary::default(),
        };

        assert_eq!(eval_exit_code(true, &run), 0);

        run.results.push(moa_eval::EvalResult {
            status: EvalStatus::Failed,
            ..moa_eval::EvalResult::default()
        });
        assert_eq!(eval_exit_code(true, &run), 1);

        run.results.push(moa_eval::EvalResult {
            status: EvalStatus::Error,
            ..moa_eval::EvalResult::default()
        });
        assert_eq!(eval_exit_code(true, &run), 2);
    }

    #[tokio::test]
    async fn memory_ingest_report_derives_name_from_filename() {
        let dir = tempdir().expect("temp dir");
        let base = dir.keep();
        let mut config = MoaConfig::default();
        config.database.url = moa_session::testing::test_database_url();
        config.local.memory_dir = base.join("memory").display().to_string();
        config.local.sandbox_dir = base.join("sandbox").display().to_string();

        let source_path = base.join("rfc-0042-auth-redesign.md");
        fs::write(&source_path, "## Topics\n- OAuth Tokens\n")
            .await
            .expect("write source");

        let report = memory_ingest_report(
            &config,
            std::slice::from_ref(&source_path),
            None,
            Some("workspace-ingest"),
        )
        .await
        .expect("memory ingest report");
        assert!(report.contains("Ingested \"Rfc 0042 Auth Redesign\""));
        assert!(report.contains("Created: sources/rfc-0042-auth-redesign.md"));

        let store = FileMemoryStore::from_config(&config)
            .await
            .expect("memory store");
        let source_page = store
            .read_page(
                &MemoryScope::Workspace {
                    workspace_id: WorkspaceId::new("workspace-ingest"),
                },
                &MemoryPath::new("sources/rfc-0042-auth-redesign.md"),
            )
            .await
            .expect("source page");
        assert!(source_page.content.contains("## Raw source"));
    }

    #[tokio::test]
    async fn memory_ingest_report_rejects_name_for_multiple_files() {
        let dir = tempdir().expect("temp dir");
        let base = dir.keep();
        let mut config = MoaConfig::default();
        config.database.url = moa_session::testing::test_database_url();
        config.local.memory_dir = base.join("memory").display().to_string();
        config.local.sandbox_dir = base.join("sandbox").display().to_string();

        let first = base.join("a.md");
        let second = base.join("b.md");
        fs::write(&first, "# A").await.expect("write first");
        fs::write(&second, "# B").await.expect("write second");

        let error = memory_ingest_report(&config, &[first, second], Some("Shared"), None)
            .await
            .expect_err("batch ingest with name should fail");
        assert!(error.to_string().contains("--name can only be used"));
    }
}

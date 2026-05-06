//! CLI entry point for MOA subcommands and daemon management.

mod api;
mod commands;
mod daemon;
mod exec;

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
    SessionFilter, SessionId, SessionStatus, SessionStore, TelemetryConfig, UserId, WorkspaceId,
    default_log_path, init_observability, metrics_endpoint_url,
};
use moa_eval::{
    AgentConfig, EngineOptions, EvalEngine, EvalRun, EvalStatus, EvaluatorOptions, ReporterOptions,
    build_evaluators, build_reporters, discover_suites, evaluate_run, list_datasets,
    load_agent_config, load_suite, register_dataset, replay_dataset_live,
};
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
use commands::privacy::{PrivacyCommand, handle_privacy_command};
use commands::skills::{SkillsCommand, handle_skills_command};
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
    /// Explains one lineage session or turn from the TimescaleDB hot store.
    Explain {
        /// Session id or turn id to inspect.
        id: String,
    },
    /// Lineage hot/cold tier query operations.
    Lineage {
        #[command(subcommand)]
        command: LineageCommand,
    },
    /// Runs graph-memory retrieval directly.
    Retrieve(RetrieveArgs),
    /// Skill import, export, and listing operations.
    Skills {
        #[command(subcommand)]
        command: SkillsCommand,
    },
    /// Privacy administration operations.
    Privacy {
        #[command(subcommand)]
        command: PrivacyCommand,
    },
    /// Promotes a workspace from pgvector to Turbopuffer.
    PromoteWorkspace(PromoteWorkspaceArgs),
    /// Rolls a workspace vector promotion back to pgvector.
    RollbackPromotion(WorkspacePromotionArgs),
    /// Finalizes a completed workspace vector promotion.
    FinalizePromotion(WorkspacePromotionArgs),
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
    /// Searches workspace memory using hybrid graph retrieval.
    Search {
        /// Search query.
        query: String,
        /// Maximum number of hits to return.
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Displays one memory node by uid, with immediate neighbors.
    Show {
        /// Node uid.
        uid: String,
    },
    /// Ingests one or more documents into workspace memory through graph ingestion.
    Ingest(IngestArgs),
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

/// Arguments for `moa retrieve`.
#[derive(Debug, Args)]
struct RetrieveArgs {
    /// Search query.
    query: String,
    /// Print full ranking details.
    #[arg(long)]
    debug: bool,
    /// Do not wait for durable lineage flush; print in-memory debug output.
    #[arg(long)]
    no_flush_wait: bool,
    /// Maximum number of hits to return.
    #[arg(long, default_value_t = 10)]
    limit: usize,
}

/// Lineage CLI commands.
#[derive(Debug, Subcommand)]
enum LineageCommand {
    /// Runs a read-only SQL query against the lineage tier.
    Query(LineageQueryArgs),
}

/// Arguments for `moa lineage query`.
#[derive(Debug, Args)]
struct LineageQueryArgs {
    /// SELECT query. Use `FROM lineage` as the logical source table.
    sql: String,
    /// Query cold Parquet objects instead of the hot TimescaleDB store.
    #[arg(long)]
    cold: bool,
    /// Postgres interval for the hot-tier time window.
    #[arg(long, default_value = "24 hours")]
    since: String,
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
    /// Registers or lists replay datasets.
    Datasets {
        #[command(subcommand)]
        command: EvalDatasetsCommand,
    },
    /// Replays a stored dataset and records score rows.
    Replay(EvalReplayArgs),
    /// Shows score summaries for one replay run.
    Scores(EvalScoresArgs),
    /// Compares score means between two replay runs.
    Compare(EvalCompareArgs),
    /// Runs the regression suite for one workspace skill.
    Skill(EvalSkillArgs),
    /// Lists discoverable eval suites in a directory.
    List {
        /// Directory to scan for suites.
        #[arg(default_value = "tests/suites")]
        dir: PathBuf,
    },
}

/// Eval dataset commands.
#[derive(Debug, Subcommand)]
enum EvalDatasetsCommand {
    /// Registers a JSONL dataset.
    Register(EvalDatasetRegisterArgs),
    /// Lists registered datasets.
    List,
}

/// Arguments for `moa eval datasets register`.
#[derive(Debug, Args)]
struct EvalDatasetRegisterArgs {
    /// JSONL dataset path.
    path: PathBuf,
    /// Dataset name.
    #[arg(long)]
    name: String,
}

/// Arguments for `moa eval replay`.
#[derive(Debug, Args)]
struct EvalReplayArgs {
    /// Dataset identifier.
    #[arg(long)]
    dataset: Uuid,
    /// Optional replay run identifier.
    #[arg(long)]
    run_id: Option<Uuid>,
    /// Maximum dataset items to replay.
    #[arg(long)]
    limit: Option<usize>,
    /// Optional embedder label for the run.
    #[arg(long)]
    embedder: Option<String>,
    /// Optional model label for the run.
    #[arg(long)]
    model: Option<String>,
}

/// Arguments for `moa eval scores`.
#[derive(Debug, Args)]
struct EvalScoresArgs {
    /// Replay run identifier.
    #[arg(long)]
    run_id: Uuid,
}

/// Arguments for `moa eval compare`.
#[derive(Debug, Args)]
struct EvalCompareArgs {
    /// Baseline replay run identifier.
    #[arg(long)]
    base_run: Uuid,
    /// New replay run identifier.
    #[arg(long)]
    new_run: Uuid,
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
            MemoryCommand::Search { query, limit } => {
                print!("{}", memory_search_report(&config, &query, limit).await?);
            }
            MemoryCommand::Show { uid } => {
                print!("{}", memory_show_report(&config, &uid).await?);
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
        },
        Some(CommandKind::Explain { id }) => {
            print!("{}", explain_report(&config, &id).await?);
        }
        Some(CommandKind::Lineage { command }) => match command {
            LineageCommand::Query(args) => {
                print!("{}", lineage_query_report(&config, &args).await?);
            }
        },
        Some(CommandKind::Retrieve(args)) => {
            print!("{}", retrieve_report(&config, &args).await?);
        }
        Some(CommandKind::Skills { command }) => {
            print!("{}", handle_skills_command(&config, command).await?);
        }
        Some(CommandKind::Privacy { command }) => {
            print!("{}", handle_privacy_command(&config, command).await?);
        }
        Some(CommandKind::PromoteWorkspace(args)) => {
            print!(
                "{}",
                handle_admin_command(&config, AdminCommand::PromoteWorkspace(args)).await?
            );
        }
        Some(CommandKind::RollbackPromotion(args)) => {
            print!(
                "{}",
                handle_admin_command(&config, AdminCommand::RollbackPromotion(args)).await?
            );
        }
        Some(CommandKind::FinalizePromotion(args)) => {
            print!(
                "{}",
                handle_admin_command(&config, AdminCommand::FinalizePromotion(args)).await?
            );
        }
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
            EvalCommand::Datasets { command } => {
                print!("{}", handle_eval_datasets(&config, command).await?);
            }
            EvalCommand::Replay(args) => {
                print!("{}", handle_eval_replay(&config, args).await?);
            }
            EvalCommand::Scores(args) => {
                print!("{}", handle_eval_scores(&config, args).await?);
            }
            EvalCommand::Compare(args) => {
                print!("{}", handle_eval_compare(&config, args).await?);
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
    let _graph_store = Arc::new(load_graph_store(&config).await?);
    let _ = args;
    bail!("moa eval skill is pending the C04 graph-native skill regression migration");
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

async fn handle_eval_datasets(config: &MoaConfig, command: EvalDatasetsCommand) -> Result<String> {
    let store = load_session_store(config).await?;
    match command {
        EvalDatasetsCommand::Register(args) => {
            let dataset_id = register_dataset(store.pool(), &args.path, &args.name)
                .await
                .context("registering eval dataset")?;
            Ok(format!(
                "dataset: {dataset_id}\nname: {}\npath: {}\n",
                args.name,
                args.path.display()
            ))
        }
        EvalDatasetsCommand::List => {
            let rows = list_datasets(store.pool())
                .await
                .context("listing eval datasets")?;
            let mut report = String::from("dataset_id\tname\titems\n");
            for (dataset_id, name, items) in rows {
                report.push_str(&format!("{dataset_id}\t{name}\t{items}\n"));
            }
            Ok(report)
        }
    }
}

async fn handle_eval_replay(config: &MoaConfig, args: EvalReplayArgs) -> Result<String> {
    let store = load_session_store(config).await?;
    let (sink, writer) = MpscSink::spawn(
        MpscSinkConfig::from(&config.observability.lineage),
        store.pool().clone(),
    )
    .await
    .context("starting lineage writer for eval replay")?;
    let run_id = args.run_id.unwrap_or_else(Uuid::now_v7);
    let report = replay_dataset_live(
        config.clone(),
        store.pool(),
        Arc::new(sink) as Arc<dyn moa_lineage_core::LineageSink>,
        moa_eval::ReplayConfig {
            dataset_id: args.dataset,
            run_id,
            model_override: args.model,
            embedder_override: args.embedder,
            limit: args.limit,
        },
    )
    .await
    .context("running eval replay")?;
    writer
        .shutdown()
        .await
        .context("flushing eval replay scores")?;

    Ok(format!(
        "run_id: {}\ndataset_id: {}\nitems: {}\nscores: {}\n",
        report.run_id, report.dataset_id, report.items, report.scores
    ))
}

async fn handle_eval_scores(config: &MoaConfig, args: EvalScoresArgs) -> Result<String> {
    let store = load_session_store(config).await?;
    let rows = sqlx::query(
        r#"
        SELECT name,
               value_type,
               COUNT(*)::BIGINT AS n,
               AVG(value_numeric) AS numeric_mean,
               AVG(CASE WHEN value_boolean THEN 1.0 ELSE 0.0 END)::DOUBLE PRECISION AS boolean_rate
        FROM analytics.scores
        WHERE run_id = $1
        GROUP BY name, value_type
        ORDER BY name, value_type
        "#,
    )
    .bind(args.run_id)
    .fetch_all(store.pool())
    .await?;

    let mut report = format!("run_id: {}\nname\ttype\tn\tmean_or_rate\n", args.run_id);
    for row in rows {
        let name: String = row.try_get("name")?;
        let value_type: String = row.try_get("value_type")?;
        let n: i64 = row.try_get("n")?;
        let numeric_mean: Option<f64> = row.try_get("numeric_mean")?;
        let boolean_rate: Option<f64> = row.try_get("boolean_rate")?;
        let value = numeric_mean.or(boolean_rate).unwrap_or(0.0);
        report.push_str(&format!("{name}\t{value_type}\t{n}\t{value:.4}\n"));
    }
    Ok(report)
}

async fn handle_eval_compare(config: &MoaConfig, args: EvalCompareArgs) -> Result<String> {
    let store = load_session_store(config).await?;
    let rows = sqlx::query(
        r#"
        WITH base AS (
            SELECT name, AVG(value_numeric) AS mean
            FROM analytics.scores
            WHERE run_id = $1 AND value_type = 'numeric'
            GROUP BY name
        ),
        new AS (
            SELECT name, AVG(value_numeric) AS mean
            FROM analytics.scores
            WHERE run_id = $2 AND value_type = 'numeric'
            GROUP BY name
        )
        SELECT COALESCE(base.name, new.name) AS name,
               base.mean AS base_mean,
               new.mean AS new_mean,
               COALESCE(new.mean, 0.0) - COALESCE(base.mean, 0.0) AS delta
        FROM base
        FULL OUTER JOIN new USING (name)
        ORDER BY name
        "#,
    )
    .bind(args.base_run)
    .bind(args.new_run)
    .fetch_all(store.pool())
    .await?;

    let mut report = format!(
        "base_run: {}\nnew_run: {}\nname\tbase\tnew\tdelta\n",
        args.base_run, args.new_run
    );
    for row in rows {
        let name: String = row.try_get("name")?;
        let base_mean: Option<f64> = row.try_get("base_mean")?;
        let new_mean: Option<f64> = row.try_get("new_mean")?;
        let delta: f64 = row.try_get("delta")?;
        report.push_str(&format!(
            "{name}\t{}\t{}\t{delta:.4}\n",
            format_optional_f64(base_mean),
            format_optional_f64(new_mean)
        ));
    }
    Ok(report)
}

fn format_optional_f64(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.4}"))
        .unwrap_or_else(|| "-".to_string())
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

async fn memory_search_report(config: &MoaConfig, query: &str, limit: usize) -> Result<String> {
    let graph = load_graph_store(config).await?;
    let seed_limit = i64::try_from(limit.max(1)).context("memory search limit is too large")?;
    let seeds = graph
        .lookup_seeds(query, seed_limit)
        .await?
        .into_iter()
        .map(|row| row.uid)
        .collect::<Vec<_>>();
    let retriever = load_hybrid_retriever(config).await?;
    let hits = retriever
        .retrieve(RetrievalRequest {
            seeds,
            query_text: query.to_string(),
            query_embedding: Vec::new(),
            scope: MemoryScope::Workspace {
                workspace_id: current_workspace_id(),
            },
            label_filter: None,
            max_pii_class: PiiClass::Restricted,
            k_final: limit,
            use_reranker: true,
            strategy: None,
        })
        .await?;

    let mut report = String::new();
    if hits.is_empty() {
        report.push_str("no hits\n");
        return Ok(report);
    }

    report.push_str("uid\tlabel\tname\tscore\tsnippet\n");
    for hit in hits {
        report.push_str(&format!(
            "{}\t{}\t{}\t{:.3}\t{}\n",
            hit.uid,
            hit.node.label.as_str(),
            sanitize_table_cell(&hit.node.name),
            hit.score,
            sanitize_table_cell(&node_snippet(&hit.node))
        ));
    }
    Ok(report)
}

async fn retrieve_report(config: &MoaConfig, args: &RetrieveArgs) -> Result<String> {
    if !args.debug {
        return memory_search_report(config, &args.query, args.limit).await;
    }

    memory_retrieve_debug_report(config, &args.query, args.limit, args.no_flush_wait).await
}

async fn memory_retrieve_debug_report(
    config: &MoaConfig,
    query: &str,
    limit: usize,
    no_flush_wait: bool,
) -> Result<String> {
    let graph = load_graph_store(config).await?;
    let seed_limit = i64::try_from(limit.max(1)).context("retrieve limit is too large")?;
    let seeds = graph
        .lookup_seeds(query, seed_limit)
        .await?
        .into_iter()
        .map(|row| row.uid)
        .collect::<Vec<_>>();
    let retriever = load_hybrid_retriever(config).await?;
    let hits = retriever
        .retrieve(RetrievalRequest {
            seeds,
            query_text: query.to_string(),
            query_embedding: Vec::new(),
            scope: MemoryScope::Workspace {
                workspace_id: current_workspace_id(),
            },
            label_filter: None,
            max_pii_class: PiiClass::Restricted,
            k_final: limit,
            use_reranker: true,
            strategy: None,
        })
        .await?;
    let lineage_turn = if config.observability.lineage.enabled && !no_flush_wait {
        Some(record_debug_retrieval_lineage(config, query, &hits).await?)
    } else {
        None
    };

    let mut report = String::new();
    report.push_str("# retrieval debug\n");
    report.push_str(&format!("query: {query}\n"));
    report.push_str(&format!(
        "lineage_enabled: {}\n",
        config.observability.lineage.enabled
    ));
    report.push_str(&format!("no_flush_wait: {no_flush_wait}\n\n"));
    if let Some(turn_id) = lineage_turn {
        report.push_str(&format!("lineage_turn: {}\n\n", turn_id.0));
    }
    if hits.is_empty() {
        report.push_str("no hits\n");
        return Ok(report);
    }

    report.push_str("rank\tuid\tlabel\tname\tscore\tlegs\tsnippet\n");
    for (rank, hit) in hits.iter().enumerate() {
        report.push_str(&format!(
            "{}\t{}\t{}\t{}\t{:.3}\t{}\t{}\n",
            rank + 1,
            hit.uid,
            hit.node.label.as_str(),
            sanitize_table_cell(&hit.node.name),
            hit.score,
            leg_trace(hit.legs),
            sanitize_table_cell(&node_snippet(&hit.node))
        ));
    }
    Ok(report)
}

async fn record_debug_retrieval_lineage(
    config: &MoaConfig,
    query: &str,
    hits: &[moa_brain::retrieval::RetrievalHit],
) -> Result<TurnId> {
    let store = load_session_store(config).await?;
    let (sink, writer) = MpscSink::spawn(
        MpscSinkConfig::from(&config.observability.lineage),
        store.pool().clone(),
    )
    .await
    .context("starting lineage writer for retrieve --debug")?;
    let turn_id = TurnId::new_v7();
    let record = RetrievalLineage {
        turn_id,
        session_id: SessionId::new(),
        workspace_id: current_workspace_id(),
        user_id: current_user_id(),
        scope: MemoryScope::Workspace {
            workspace_id: current_workspace_id(),
        },
        ts: Utc::now(),
        query_original: query.to_string(),
        query_expansions: Vec::new(),
        vector_hits: hits
            .iter()
            .map(|hit| VecHit {
                chunk_id: hit.uid,
                score: hit.score as f32,
                source: "hybrid".to_string(),
                embedder: "debug".to_string(),
                embed_dim: memory_vector::VECTOR_DIMENSION as u16,
            })
            .collect(),
        graph_paths: Vec::new(),
        fusion_scores: hits
            .iter()
            .map(|hit| FusedHit {
                chunk_id: hit.uid,
                fused_score: hit.score as f32,
                vector_contribution: if hit.legs.vector { 1.0 } else { 0.0 },
                graph_contribution: if hit.legs.graph { 1.0 } else { 0.0 },
                lexical_contribution: if hit.legs.lexical { 1.0 } else { 0.0 },
                fusion_method: "rrf".to_string(),
            })
            .collect(),
        rerank_scores: hits
            .iter()
            .enumerate()
            .map(|(idx, hit)| RerankHit {
                chunk_id: hit.uid,
                original_index: idx.min(u16::MAX as usize) as u16,
                relevance_score: hit.score as f32,
                rerank_model: "debug".to_string(),
            })
            .collect(),
        top_k: hits.iter().map(|hit| hit.uid).collect(),
        timings: StageTimings::default(),
        introspection: BackendIntrospection::default(),
        stage: RetrievalStage::Single,
    };
    let json = serde_json::to_value(LineageEvent::Retrieval(record))
        .context("serializing retrieve --debug lineage")?;
    sink.record(json);
    writer
        .shutdown()
        .await
        .context("flushing retrieve --debug lineage")?;
    Ok(turn_id)
}

async fn explain_report(config: &MoaConfig, id: &str) -> Result<String> {
    let id = Uuid::parse_str(id).with_context(|| format!("invalid session or turn id `{id}`"))?;
    let store = load_session_store(config).await?;
    let rows = sqlx::query(
        r#"
        SELECT turn_id, ts, record_kind, payload
        FROM analytics.turn_lineage
        WHERE session_id = $1 OR turn_id = $1
        ORDER BY ts ASC, record_kind ASC
        "#,
    )
    .bind(id)
    .fetch_all(store.pool())
    .await?;

    let mut report = String::new();
    if rows.is_empty() {
        report.push_str("no lineage records\n");
        return Ok(report);
    }

    let mut last_turn: Option<Uuid> = None;
    for row in rows {
        let turn_id: Uuid = row.try_get("turn_id")?;
        let ts: chrono::DateTime<Utc> = row.try_get("ts")?;
        let record_kind: i16 = row.try_get("record_kind")?;
        let payload: serde_json::Value = row.try_get("payload")?;
        if Some(turn_id) != last_turn {
            report.push_str(&format!("\n=== turn {turn_id}  {ts}\n"));
            last_turn = Some(turn_id);
        }
        render_lineage_record(record_kind, &payload, &mut report);
    }
    Ok(report)
}

async fn lineage_query_report(config: &MoaConfig, args: &LineageQueryArgs) -> Result<String> {
    if args.cold {
        anyhow::bail!("cold lineage query is not configured in this CLI build");
    }
    let prepared = prepare_lineage_sql(&args.sql)?;
    let store = load_session_store(config).await?;
    let mut tx = store.pool().begin().await?;
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout = '5s'")
        .execute(&mut *tx)
        .await?;
    let rows: serde_json::Value = sqlx::query_scalar(&format!(
        "SELECT COALESCE(jsonb_agg(row_to_json(lineage_query)), '[]'::jsonb) \
         FROM ({prepared}) lineage_query"
    ))
    .bind(&args.since)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    serde_json::to_string_pretty(&rows).map_err(Into::into)
}

fn prepare_lineage_sql(sql: &str) -> Result<String> {
    let trimmed = sql.trim();
    let lower = trimmed.to_ascii_lowercase();
    if !(lower.starts_with("select ") || lower.starts_with("with ")) {
        anyhow::bail!("only SELECT or WITH queries are permitted");
    }
    if trimmed.contains(';') {
        anyhow::bail!("semicolon-separated statements are not permitted");
    }
    let Some(idx) = lower.find("from lineage") else {
        anyhow::bail!("query must use `FROM lineage` as the source table");
    };
    let replacement = "FROM (SELECT * FROM analytics.turn_lineage WHERE ts > now() - ($1::text)::interval) lineage";
    let mut prepared = String::with_capacity(trimmed.len() + replacement.len());
    prepared.push_str(&trimmed[..idx]);
    prepared.push_str(replacement);
    prepared.push_str(&trimmed[idx + "from lineage".len()..]);
    Ok(prepared)
}

#[cfg(test)]
mod lineage_query_tests {
    use super::prepare_lineage_sql;

    #[test]
    fn prepare_lineage_sql_replaces_logical_lineage_source() {
        let sql = prepare_lineage_sql("SELECT count(*) FROM lineage WHERE record_kind = 4")
            .expect("lineage query should prepare");

        assert!(sql.contains("analytics.turn_lineage"));
        assert!(sql.contains("record_kind = 4"));
    }

    #[test]
    fn prepare_lineage_sql_rejects_mutating_statement() {
        let error = prepare_lineage_sql("DELETE FROM lineage")
            .expect_err("mutating lineage query should fail");

        assert!(error.to_string().contains("only SELECT"));
    }
}

fn render_lineage_record(kind: i16, payload: &serde_json::Value, out: &mut String) {
    let record = payload.get("record").unwrap_or(payload);
    match kind {
        1 => render_retrieval_record(record, out),
        2 => render_context_record(record, out),
        3 => render_generation_record(record, out),
        4 => render_citation_record(record, out),
        _ => {}
    }
}

fn render_retrieval_record(record: &serde_json::Value, out: &mut String) {
    let query = record
        .get("query_original")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let total_ms = record
        .pointer("/timings/total_ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let top_k = record
        .get("top_k")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    out.push_str(&format!(
        "retrieval: query=\"{query}\" top_k={top_k} total_ms={total_ms}\n"
    ));
}

fn render_context_record(record: &serde_json::Value, out: &mut String) {
    let chunks = record
        .get("chunks_in_window")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let tokens = record
        .get("total_input_tokens_estimated")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    out.push_str(&format!(
        "context: chunks={chunks} estimated_input_tokens={tokens}\n"
    ));
}

fn render_generation_record(record: &serde_json::Value, out: &mut String) {
    let provider = record
        .get("provider")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let model = record
        .get("response_model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let input = record
        .pointer("/usage/input_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let output = record
        .pointer("/usage/output_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    out.push_str(&format!(
        "generation: provider={provider} model={model} input_tokens={input} output_tokens={output}\n"
    ));
}

fn render_citation_record(record: &serde_json::Value, out: &mut String) {
    let vendor = record
        .get("vendor_used")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let verifier = record
        .get("verifier_used")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let citations = record
        .get("citations")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    out.push_str(&format!(
        "citation: vendor={vendor} verifier={verifier} citations={citations}\n"
    ));
}

fn leg_trace(legs: moa_brain::retrieval::LegSources) -> String {
    let mut out = Vec::new();
    if legs.graph {
        out.push("graph");
    }
    if legs.vector {
        out.push("vector");
    }
    if legs.lexical {
        out.push("lexical");
    }
    if out.is_empty() {
        "none".to_string()
    } else {
        out.join("+")
    }
}

async fn memory_show_report(config: &MoaConfig, uid_str: &str) -> Result<String> {
    let uid = Uuid::parse_str(uid_str).with_context(|| format!("invalid node uid `{uid_str}`"))?;
    let store = load_graph_store(config).await?;
    let node = store
        .get_node(uid)
        .await?
        .with_context(|| format!("node {uid} not found"))?;
    let neighbors = store.neighbors(uid, 1, None).await.unwrap_or_default();
    let properties = node
        .properties_summary
        .unwrap_or_else(|| serde_json::json!({}));

    let mut report = format!(
        "uid: {}\nlabel: {}\nname: {}\nscope: {}\nvalid_from: {}\nvalid_to: {}\n\nproperties:\n{}\n",
        node.uid,
        node.label.as_str(),
        node.name,
        node.scope,
        node.valid_from.to_rfc3339(),
        node.valid_to
            .map(|timestamp| timestamp.to_rfc3339())
            .unwrap_or_else(|| "<open>".to_string()),
        serde_json::to_string_pretty(&properties)?,
    );
    if !neighbors.is_empty() {
        report.push_str("\nneighbors:\n");
        for neighbor in neighbors {
            report.push_str(&format!(
                "- {} {} {}\n",
                neighbor.uid,
                neighbor.label.as_str(),
                neighbor.name
            ));
        }
    }
    Ok(report)
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

    let workspace_id = workspace
        .map(resolve_workspace_arg)
        .unwrap_or_else(current_workspace_id);
    let vo = load_ingestion_vo(config).await?;

    let mut sections = Vec::with_capacity(files.len());
    for file in files {
        let content = fs::read_to_string(file)
            .await
            .with_context(|| format!("reading {}", file.display()))?;
        let source_name = match name {
            Some(value) => value.to_string(),
            None => derive_ingest_source_name(file),
        };
        let turn = synthesize_cli_ingest_turn(&workspace_id, &source_name, &content);
        let report = vo.ingest_turn(turn).await?;
        sections.push(format_cli_ingest_section(file, &source_name, &report));
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
        format!("graph_memory: {}", graph_memory_status(config).await),
        format!("lineage: {}", lineage_status(config).await),
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

async fn lineage_status(config: &MoaConfig) -> String {
    if !config.observability.lineage.enabled {
        return "disabled".to_string();
    }

    match load_session_store(config).await {
        Ok(store) => {
            match sqlx::query_scalar::<_, i64>("SELECT count(*) FROM analytics.turn_lineage")
                .fetch_one(store.pool())
                .await
            {
                Ok(count) => format!(
                    "enabled rows={} journal={}",
                    count, config.observability.lineage.journal_path
                ),
                Err(error) => format!("enabled schema_unavailable ({error})"),
            }
        }
        Err(error) => format!("enabled database_unavailable ({error})"),
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

async fn load_graph_store(config: &MoaConfig) -> Result<AgeGraphStore> {
    let session_store = load_session_store(config).await?;
    let scope = ScopeContext::workspace(current_workspace_id());
    Ok(AgeGraphStore::scoped(session_store.pool().clone(), scope))
}

async fn load_hybrid_retriever(config: &MoaConfig) -> Result<HybridRetriever> {
    let session_store = load_session_store(config).await?;
    let pool = session_store.pool().clone();
    let scope = ScopeContext::workspace(current_workspace_id());
    let vector = Arc::new(PgvectorStore::new(pool.clone(), scope.clone()));
    let graph = AgeGraphStore::scoped(pool.clone(), scope).with_vector_store(vector.clone());
    Ok(HybridRetriever::from_env(pool, Arc::new(graph), vector))
}

async fn load_ingestion_vo(config: &MoaConfig) -> Result<CliIngestionVo> {
    let session_store = load_session_store(config).await?;
    Ok(CliIngestionVo::new(session_store.pool().clone()))
}

struct CliIngestionVo {
    pool: sqlx::PgPool,
}

impl CliIngestionVo {
    fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }

    async fn ingest_turn(&self, turn: SessionTurn) -> Result<IngestApplyReport> {
        let _ = memory_ingest::install_runtime_with_pool(self.pool.clone());
        memory_ingest::ingest_turn_direct(turn)
            .await
            .map_err(|error| anyhow::anyhow!("{error:?}"))
    }
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

fn current_user_id() -> UserId {
    UserId::new(
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

fn synthesize_cli_ingest_turn(
    workspace_id: &WorkspaceId,
    source_name: &str,
    content: &str,
) -> SessionTurn {
    SessionTurn {
        workspace_id: workspace_id.clone(),
        user_id: current_user_id(),
        session_id: SessionId::new(),
        turn_seq: 1,
        transcript: format!("source: {source_name}\n\n{content}"),
        dominant_pii_class: "none".to_string(),
        finalized_at: Utc::now(),
    }
}

fn format_cli_ingest_section(path: &Path, source_name: &str, report: &IngestApplyReport) -> String {
    let mut lines = vec![
        format!("Ingested \"{}\" ({})", source_name, path.display()),
        format!(
            "nodes: inserted={} superseded={} skipped={} failed={}",
            report.inserted, report.superseded, report.skipped, report.failed
        ),
        "edges: 0".to_string(),
        "contradictions: 0".to_string(),
    ];

    if report.failed > 0 {
        lines.push("dead_lettered: see moa.ingest_dlq".to_string());
    }

    lines.join("\n")
}

fn node_snippet(node: &memory_graph::NodeIndexRow) -> String {
    let Some(properties) = &node.properties_summary else {
        return String::new();
    };
    if let Some(value) = properties
        .get("summary")
        .and_then(serde_json::Value::as_str)
    {
        return value.to_string();
    }
    if let Some(value) = properties.get("object").and_then(serde_json::Value::as_str) {
        return value.to_string();
    }
    properties.to_string()
}

fn sanitize_table_cell(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('\t', " ")
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

async fn graph_memory_status(config: &MoaConfig) -> String {
    let workspace_id = current_workspace_id();
    match load_session_store(config).await {
        Ok(store) => {
            let status = sqlx::query_as::<_, (i64, Option<String>)>(
                r#"
                SELECT count(*)::bigint, max(created_at)::text
                FROM moa.node_index
                WHERE workspace_id = $1
                  AND valid_to IS NULL
                "#,
            )
            .bind(workspace_id.as_str())
            .fetch_one(store.pool())
            .await;

            match status {
                Ok((count, Some(last_write))) => {
                    format!("healthy ({count} nodes in current workspace; last_write={last_write})")
                }
                Ok((count, None)) => {
                    format!("healthy ({count} nodes in current workspace; last_write=none)")
                }
                Err(error) => format!("unhealthy ({error})"),
            }
        }
        Err(error) => format!("unhealthy ({error})"),
    }
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
        "memory.vector.embedder.name" => config.memory.vector.embedder.name = value.to_string(),
        "memory.vector.embedder.output_dim" => {
            config.memory.vector.embedder.output_dim =
                value.parse().context("expected integer output dimension")?;
        }
        "memory.vector.embedder.cohere.api_key_env" => {
            config.memory.vector.embedder.cohere.api_key_env = value.to_string();
        }
        "memory.vector.embedder.gemini.api_key_env" => {
            config.memory.vector.embedder.gemini.api_key_env = value.to_string();
        }
        "memory.vector.embedder.gemini.default_role" => {
            config.memory.vector.embedder.gemini.default_role = value.to_string();
        }
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
    use super::memory_graph::{GraphStore, NodeLabel, NodeWriteIntent, PiiClass};
    use super::memory_ingest::IngestApplyReport;
    use super::{
        apply_config_update, default_log_path, doctor_report, eval_exit_code,
        format_cli_ingest_section, memory_ingest_report, memory_search_report, memory_show_report,
        parse_bool, synthesize_cli_ingest_turn, version_text,
    };
    use chrono::Utc;
    use moa_core::{MoaConfig, SessionId, WorkspaceId};
    use moa_eval::{EvalRun, EvalStatus, RunSummary};
    use serde_json::json;
    use tempfile::tempdir;
    use tokio::fs;
    use uuid::Uuid;

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

    #[test]
    fn cli_ingest_turn_carries_workspace_source_and_content() {
        let workspace_id = WorkspaceId::new("workspace-ingest");
        let turn =
            synthesize_cli_ingest_turn(&workspace_id, "Auth Redesign", "Fact: auth uses JWT");

        assert_eq!(turn.workspace_id, workspace_id);
        assert_eq!(turn.turn_seq, 1);
        assert!(turn.transcript.contains("source: Auth Redesign"));
        assert!(turn.transcript.contains("Fact: auth uses JWT"));
        assert_eq!(turn.dominant_pii_class, "none");
    }

    #[test]
    fn cli_ingest_section_reports_graph_counts() {
        let report = IngestApplyReport {
            inserted: 2,
            superseded: 1,
            skipped: 3,
            failed: 0,
        };

        let section =
            format_cli_ingest_section(std::path::Path::new("sample.md"), "Sample", &report);

        assert!(section.contains("Ingested \"Sample\" (sample.md)"));
        assert!(section.contains("nodes: inserted=2 superseded=1 skipped=3 failed=0"));
        assert!(section.contains("edges: 0"));
        assert!(section.contains("contradictions: 0"));
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

    #[tokio::test]
    #[ignore = "requires graph test database with AGE, sidecar, and pgvector migrations"]
    async fn memory_ingest_report_graph_smoke() {
        let dir = tempdir().expect("temp dir");
        let base = dir.keep();
        let mut config = MoaConfig::default();
        config.database.url = moa_session::testing::test_database_url();
        config.local.sandbox_dir = base.join("sandbox").display().to_string();

        let source_path = base.join("rfc-0042-auth-redesign.md");
        fs::write(
            &source_path,
            "Fact: auth service uses JWT for session tokens",
        )
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
        assert!(report.contains("nodes:"));
        assert!(report.contains("edges:"));
    }

    #[tokio::test]
    #[ignore = "requires graph test database with AGE, sidecar, and pgvector migrations"]
    async fn memory_search_report_empty_graph_smoke() {
        let mut config = MoaConfig::default();
        config.database.url = moa_session::testing::test_database_url();

        let report = memory_search_report(&config, "unlikely-empty-search", 10)
            .await
            .expect("memory search report");

        assert!(report == "no hits\n" || report.starts_with("uid\tlabel\tname\tscore\tsnippet\n"));
    }

    #[tokio::test]
    #[ignore = "requires graph test database with AGE, sidecar, and pgvector migrations"]
    async fn memory_show_report_seeded_node_smoke() {
        let mut config = MoaConfig::default();
        config.database.url = moa_session::testing::test_database_url();
        let store = super::load_graph_store(&config)
            .await
            .expect("load graph store");
        let uid = Uuid::now_v7();
        store
            .create_node(NodeWriteIntent {
                uid,
                label: NodeLabel::Fact,
                workspace_id: Some(super::current_workspace_id().to_string()),
                user_id: None,
                scope: "workspace".to_string(),
                name: "seeded cli memory fact".to_string(),
                properties: json!({
                    "summary": "seeded cli memory fact",
                    "source_session_id": SessionId::new().to_string(),
                }),
                pii_class: PiiClass::None,
                confidence: Some(1.0),
                valid_from: Utc::now(),
                embedding: None,
                embedding_model: None,
                embedding_model_version: None,
                actor_id: "test".to_string(),
                actor_kind: "system".to_string(),
            })
            .await
            .expect("seed graph node");

        let report = memory_show_report(&config, &uid.to_string())
            .await
            .expect("memory show report");

        assert!(report.contains(&format!("uid: {uid}")));
        assert!(report.contains("label: Fact"));
    }
}

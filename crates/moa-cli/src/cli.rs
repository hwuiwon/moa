//! Clap argument tree for the `moa` binary.

use super::*;

/// Top-level MOA command line interface.
#[derive(Debug, Parser)]
#[command(name = "moa", about = "MOA terminal agent client", version)]
pub(crate) struct Cli {
    /// Enable debug logging to a file instead of the terminal.
    #[arg(long)]
    pub(crate) debug: bool,

    /// Override the debug log file path.
    #[arg(long, value_name = "PATH")]
    pub(crate) log_file: Option<PathBuf>,

    /// Runs one prompt and prints the final assistant response when no subcommand is supplied.
    #[arg(value_name = "PROMPT")]
    pub(crate) prompt: Option<String>,

    #[command(subcommand)]
    pub(crate) command: Option<CommandKind>,
}

/// Supported CLI subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum CommandKind {
    /// Runs one prompt and prints the final assistant response to stdout.
    Exec(ExecArgs),
    /// Shows orchestrator and session status.
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
    /// Authentication and API-key operations.
    Auth(AuthCommand),
    /// Builtin approval operations.
    Approvals(ApprovalsCommand),
    /// Agent template and agent principal operations.
    Agents(AgentsCommand),
    /// Authorization tuple administration.
    Authz(AuthzCommand),
    /// Tenant audit administration.
    Tenants(TenantsCommand),
    /// OCSF security-audit operations.
    Audit(AuditCommand),
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
    /// Inspects the configured orchestrator endpoint.
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
pub(crate) struct ExecArgs {
    /// Prompt text to submit.
    #[arg(required = true)]
    pub(crate) prompt: String,
}

/// Session-list filtering arguments.
#[derive(Debug, Args)]
pub(crate) struct SessionsArgs {
    /// Restrict sessions to one workspace id or `.` for the current directory.
    #[arg(long)]
    pub(crate) workspace: Option<String>,
}

/// Session analytics commands.
#[derive(Debug, Subcommand)]
pub(crate) enum SessionCommand {
    /// Shows summary stats for one session.
    Stats {
        /// Session id to inspect.
        id: String,
    },
}

/// Workspace analytics commands.
#[derive(Debug, Subcommand)]
pub(crate) enum WorkspaceCommand {
    /// Shows workspace rollups over a recent window.
    Stats(WorkspaceStatsArgs),
}

/// Tool analytics commands.
#[derive(Debug, Subcommand)]
pub(crate) enum ToolCommand {
    /// Shows per-tool latency and success metrics.
    Stats(ToolStatsArgs),
}

/// Cache analytics commands.
#[derive(Debug, Subcommand)]
pub(crate) enum CacheCommand {
    /// Shows cache usage trends for a workspace.
    Stats(CacheStatsArgs),
}

/// Arguments for `moa workspace stats`.
#[derive(Debug, Args)]
pub(crate) struct WorkspaceStatsArgs {
    /// Workspace id override. Use `.` for the current directory workspace.
    #[arg(long)]
    pub(crate) workspace: Option<String>,

    /// Number of days to include.
    #[arg(long, default_value_t = 30)]
    pub(crate) days: u32,
}

/// Arguments for `moa tool stats`.
#[derive(Debug, Args)]
pub(crate) struct ToolStatsArgs {
    /// Optional workspace filter. Use `.` for the current directory workspace.
    #[arg(long)]
    pub(crate) workspace: Option<String>,
}

/// Arguments for `moa cache stats`.
#[derive(Debug, Args)]
pub(crate) struct CacheStatsArgs {
    /// Workspace id override. Use `.` for the current directory workspace.
    #[arg(long)]
    pub(crate) workspace: Option<String>,

    /// Number of days to include.
    #[arg(long, default_value_t = 30)]
    pub(crate) days: u32,
}

/// Memory CLI commands.
#[derive(Debug, Subcommand)]
pub(crate) enum MemoryCommand {
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
pub(crate) struct IngestArgs {
    /// File path(s) to ingest. Shell expansion can be used for batches.
    #[arg(required = true)]
    pub(crate) files: Vec<PathBuf>,

    /// Optional source name override for a single file.
    #[arg(long)]
    pub(crate) name: Option<String>,

    /// Workspace id override. Use `.` for the current directory workspace.
    #[arg(long)]
    pub(crate) workspace: Option<String>,
}

/// Arguments for `moa retrieve`.
#[derive(Debug, Args)]
pub(crate) struct RetrieveArgs {
    /// Search query.
    pub(crate) query: String,
    /// Print full ranking details.
    #[arg(long)]
    pub(crate) debug: bool,
    /// Do not wait for durable lineage flush; print in-memory debug output.
    #[arg(long)]
    pub(crate) no_flush_wait: bool,
    /// Maximum number of hits to return.
    #[arg(long, default_value_t = 10)]
    pub(crate) limit: usize,
}

/// Lineage CLI commands.
#[derive(Debug, Subcommand)]
pub(crate) enum LineageCommand {
    /// Runs a read-only SQL query against the lineage tier.
    Query(LineageQueryArgs),
    /// Exports a DSAR lineage bundle for one subject.
    Export(LineageExportArgs),
    /// Verifies a hot compliance window or an audit root row.
    Verify(LineageVerifyArgs),
    /// Marks a subject pseudonym as erased in the PII vault.
    Erase(LineageEraseArgs),
}

/// Arguments for `moa lineage query`.
#[derive(Debug, Args)]
pub(crate) struct LineageQueryArgs {
    /// SELECT query. Use `FROM lineage` as the logical source table.
    pub(crate) sql: String,
    /// Query cold Parquet objects instead of the hot TimescaleDB store.
    #[arg(long)]
    pub(crate) cold: bool,
    /// Postgres interval for the hot-tier time window.
    #[arg(long, default_value = "24 hours")]
    pub(crate) since: String,
}

/// Arguments for `moa lineage export`.
#[derive(Debug, Args)]
pub(crate) struct LineageExportArgs {
    /// Subject pseudonym or natural identifier to search for.
    #[arg(long)]
    pub(crate) subject: String,
    /// Workspace id, or `.` for the current workspace.
    #[arg(long, default_value = ".")]
    pub(crate) workspace: String,
    /// Output zip path.
    #[arg(long)]
    pub(crate) out: PathBuf,
}

/// Arguments for `moa lineage verify`.
#[derive(Debug, Args)]
pub(crate) struct LineageVerifyArgs {
    /// `hot`, an audit root UUID, or an audit root object URI recorded in the DB.
    pub(crate) window: String,
    /// Workspace id, or `.` for the current workspace.
    #[arg(long, default_value = ".")]
    pub(crate) workspace: String,
    /// Postgres interval for `hot` verification.
    #[arg(long, default_value = "24 hours")]
    pub(crate) since: String,
}

/// Arguments for `moa lineage erase`.
#[derive(Debug, Args)]
pub(crate) struct LineageEraseArgs {
    /// Hex-encoded subject pseudonym.
    #[arg(long)]
    pub(crate) subject: String,
    /// Workspace id, or `.` for the current workspace.
    #[arg(long, default_value = ".")]
    pub(crate) workspace: String,
}

/// Config CLI commands.
#[derive(Debug, Subcommand)]
pub(crate) enum ConfigCommand {
    /// Updates a supported config key.
    Set {
        /// Dotted config key name.
        key: String,
        /// New value.
        value: String,
    },
}

/// Orchestrator endpoint diagnostic commands.
#[derive(Debug, Subcommand)]
pub(crate) enum DaemonCommand {
    /// Shows orchestrator endpoint status.
    Status,
}

/// Checkpoint CLI commands.
#[derive(Debug, Subcommand)]
pub(crate) enum CheckpointCommand {
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
pub(crate) enum EvalCommand {
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
pub(crate) enum EvalDatasetsCommand {
    /// Registers a JSONL dataset.
    Register(EvalDatasetRegisterArgs),
    /// Lists registered datasets.
    List,
}

/// Arguments for `moa eval datasets register`.
#[derive(Debug, Args)]
pub(crate) struct EvalDatasetRegisterArgs {
    /// JSONL dataset path.
    pub(crate) path: PathBuf,
    /// Dataset name.
    #[arg(long)]
    pub(crate) name: String,
}

/// Arguments for `moa eval replay`.
#[derive(Debug, Args)]
pub(crate) struct EvalReplayArgs {
    /// Dataset identifier.
    #[arg(long)]
    pub(crate) dataset: Uuid,
    /// Optional replay run identifier.
    #[arg(long)]
    pub(crate) run_id: Option<Uuid>,
    /// Maximum dataset items to replay.
    #[arg(long)]
    pub(crate) limit: Option<usize>,
    /// Optional embedder label for the run.
    #[arg(long)]
    pub(crate) embedder: Option<String>,
    /// Optional model label for the run.
    #[arg(long)]
    pub(crate) model: Option<String>,
}

/// Arguments for `moa eval scores`.
#[derive(Debug, Args)]
pub(crate) struct EvalScoresArgs {
    /// Replay run identifier.
    #[arg(long)]
    pub(crate) run_id: Uuid,
}

/// Arguments for `moa eval compare`.
#[derive(Debug, Args)]
pub(crate) struct EvalCompareArgs {
    /// Baseline replay run identifier.
    #[arg(long)]
    pub(crate) base_run: Uuid,
    /// New replay run identifier.
    #[arg(long)]
    pub(crate) new_run: Uuid,
}

/// Arguments for `moa eval run`.
#[derive(Debug, Args)]
pub(crate) struct EvalRunArgs {
    /// Path to the test suite file.
    #[arg(long)]
    pub(crate) suite: PathBuf,

    /// Paths to one or more agent config files.
    #[arg(long, required = true)]
    pub(crate) config: Vec<PathBuf>,

    /// Report sink spec: `terminal`, `json:<path>`, or `langfuse`.
    #[arg(long, default_value = "terminal")]
    pub(crate) report: Vec<String>,

    /// Maximum concurrent eval executions.
    #[arg(long, default_value_t = 1)]
    pub(crate) parallel: usize,

    /// Exit non-zero when any run fails, errors, or times out.
    #[arg(long)]
    pub(crate) ci: bool,

    /// Evaluators to run.
    #[arg(
        long,
        default_values_t = vec![
            String::from("trajectory"),
            String::from("output"),
            String::from("tool_success")
        ]
    )]
    pub(crate) evaluator: Vec<String>,

    /// Maximum allowed per-run cost in dollars.
    #[arg(long)]
    pub(crate) max_cost: Option<f64>,

    /// Maximum allowed per-run latency in milliseconds.
    #[arg(long)]
    pub(crate) max_latency: Option<u64>,

    /// Maximum allowed tokens per run.
    #[arg(long)]
    pub(crate) max_tokens: Option<usize>,

    /// Maximum allowed tool calls per run.
    #[arg(long)]
    pub(crate) max_tool_calls: Option<usize>,

    /// Maximum allowed turns per run.
    #[arg(long)]
    pub(crate) max_turns: Option<usize>,

    /// Include per-case response and score comments in terminal output.
    #[arg(long, short)]
    pub(crate) verbose: bool,
}

/// Arguments for `moa eval skill`.
#[derive(Debug, Args)]
pub(crate) struct EvalSkillArgs {
    /// Skill name, path fragment, or full memory path.
    pub(crate) skill: String,

    /// Report sink spec: `terminal`, `json:<path>`, or `langfuse`.
    #[arg(long, default_value = "terminal")]
    pub(crate) report: Vec<String>,

    /// Verbose output with per-case detail.
    #[arg(long, short)]
    pub(crate) verbose: bool,

    /// Exit non-zero when the skill suite fails.
    #[arg(long)]
    pub(crate) ci: bool,
}

/// Arguments for `moa eval plan`.
#[derive(Debug, Args)]
pub(crate) struct EvalPlanArgs {
    /// Path to the test suite file.
    #[arg(long)]
    pub(crate) suite: PathBuf,

    /// Paths to one or more agent config files.
    #[arg(long, required = true)]
    pub(crate) config: Vec<PathBuf>,
}

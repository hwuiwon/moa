//! CLI entry point for MOA subcommands, daemon management, and the TUI.

mod daemon;
mod exec;

use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use moa_core::{
    DatabaseBackend, MemoryPath, MemoryScope, MemoryStore, MoaConfig, SessionFilter, SessionId,
    SessionStatus, SessionStore, WorkspaceId, init_observability,
};
use moa_memory::FileMemoryStore;
use moa_session::{SessionDatabase, create_session_store};
use moa_tui::{RunTuiOptions, run_tui, run_tui_with_options};
use tokio::fs;
use tokio::process::Command;
use uuid::Uuid;

/// Top-level MOA command line interface.
#[derive(Debug, Parser)]
#[command(name = "moa", about = "MOA local terminal agent", version)]
struct Cli {
    /// Launch the TUI with a prompt already submitted.
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
    /// Attaches the TUI to a specific session.
    Attach {
        /// Session identifier.
        session_id: String,
    },
    /// Resumes the most recent session or a specific session in the TUI.
    Resume {
        /// Optional explicit session identifier.
        session_id: Option<String>,
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
    /// Enables or inspects cloud sync configuration.
    Sync {
        #[command(subcommand)]
        command: SyncCommand,
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

/// Sync CLI commands.
#[derive(Debug, Subcommand)]
enum SyncCommand {
    /// Enables Turso-backed cloud sync using an embedded replica.
    Enable {
        /// Turso database URL. Falls back to `TURSO_DATABASE_URL`.
        turso_url: Option<String>,
    },
}

/// Runs the `moa` CLI binary.
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = MoaConfig::load()?;
    let _telemetry = init_observability(&config)?;

    match cli.command {
        None => {
            if let Some(prompt) = cli.prompt {
                run_tui_with_options(
                    config,
                    RunTuiOptions {
                        initial_prompt: Some(prompt),
                        ..RunTuiOptions::default()
                    },
                )
                .await?;
            } else {
                run_tui(config).await?;
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
        Some(CommandKind::Attach { session_id }) => {
            let session_id = parse_session_id(&session_id)?;
            let use_daemon = daemon::daemon_info(&config).await.is_ok();
            run_tui_with_options(
                config,
                RunTuiOptions {
                    attach_session_id: Some(session_id),
                    force_daemon: use_daemon,
                    ..RunTuiOptions::default()
                },
            )
            .await?;
        }
        Some(CommandKind::Resume { session_id }) => {
            let session_id = match session_id {
                Some(session_id) => parse_session_id(&session_id)?,
                None => most_recent_session_id(&config).await?,
            };
            let use_daemon = daemon::daemon_info(&config).await.is_ok();
            run_tui_with_options(
                config,
                RunTuiOptions {
                    attach_session_id: Some(session_id),
                    force_daemon: use_daemon,
                    ..RunTuiOptions::default()
                },
            )
            .await?;
        }
        Some(CommandKind::Memory { command }) => match command {
            MemoryCommand::Search { query } => {
                print!("{}", memory_search_report(&config, &query).await?);
            }
            MemoryCommand::Show { path } => {
                print!("{}", memory_show_report(&config, &path).await?);
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
                updated.save()?;
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
            print!("{}", doctor_report(&config).await?);
        }
        Some(CommandKind::Daemon { command }) => match command {
            DaemonCommand::Start => daemon::start_daemon(&config).await?,
            DaemonCommand::Stop => daemon::stop_daemon(&config).await?,
            DaemonCommand::Status => print!("{}", daemon_status_report(&config).await?),
            DaemonCommand::Logs => print!("{}", daemon::daemon_logs(&config).await?),
            DaemonCommand::Serve => daemon::run_daemon_server(config).await?,
        },
        Some(CommandKind::Sync { command }) => match command {
            SyncCommand::Enable { turso_url } => {
                print!("{}", sync_enable_report(config, turso_url).await?);
            }
        },
    }

    Ok(())
}

/// Returns a plain-text version string.
fn version_text() -> String {
    format!("moa {}", env!("CARGO_PKG_VERSION"))
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

async fn memory_search_report(config: &MoaConfig, query: &str) -> Result<String> {
    let store = FileMemoryStore::from_config(config).await?;
    let results = store
        .search(query, MemoryScope::Workspace(current_workspace_id()), 20)
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
    let store = FileMemoryStore::from_config(config).await?;
    let path = MemoryPath::new(path);
    let page = store
        .read_page(MemoryScope::Workspace(current_workspace_id()), &path)
        .await?;
    let rendered = toml::to_string(&page.metadata).unwrap_or_default();
    Ok(format!("---\n{}---\n{}", rendered, page.content))
}

async fn doctor_report(config: &MoaConfig) -> Result<String> {
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
            "openrouter_key: {} ({})",
            env_presence(&config.providers.openrouter.api_key_env),
            config.providers.openrouter.api_key_env
        ),
        format!("docker: {}", docker_status().await),
        format!("disk: {}", disk_status(config).await),
        format!("session_db: {}", session_db_status(config).await),
        format!("cloud_sync: {}", cloud_sync_status(config).await),
        format!("memory_index: {}", memory_index_status(config).await),
    ];

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
        config.save()?;
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

async fn most_recent_session_id(config: &MoaConfig) -> Result<SessionId> {
    let sessions = load_session_store(config)
        .await?
        .list_sessions(SessionFilter {
            limit: Some(1),
            ..SessionFilter::default()
        })
        .await?;
    sessions
        .into_iter()
        .next()
        .map(|session| session.session_id)
        .context("no sessions found")
}

async fn load_session_store(config: &MoaConfig) -> Result<Arc<SessionDatabase>> {
    create_session_store(config)
        .await
        .context("opening session store")
}

fn parse_session_id(value: &str) -> Result<SessionId> {
    Ok(SessionId(
        Uuid::parse_str(value).context("invalid session id")?,
    ))
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

fn env_presence(key: &str) -> &'static str {
    if env::var(key).is_ok() {
        "present"
    } else {
        "missing"
    }
}

async fn docker_status() -> String {
    match Command::new("docker").arg("info").output().await {
        Ok(output) if output.status.success() => "available".to_string(),
        Ok(output) => format!("unhealthy (exit {})", output.status),
        Err(_) => "missing".to_string(),
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

async fn session_db_status(config: &MoaConfig) -> String {
    match load_session_store(config).await {
        Ok(store) => match store
            .list_sessions(SessionFilter {
                limit: Some(1),
                ..SessionFilter::default()
            })
            .await
        {
            Ok(_) => "healthy".to_string(),
            Err(error) => format!("unhealthy ({error})"),
        },
        Err(error) => format!("unhealthy ({error})"),
    }
}

async fn cloud_sync_status(config: &MoaConfig) -> String {
    if !(config.cloud.enabled && config.cloud.turso_url.is_some()) {
        return "disabled".to_string();
    }

    match load_session_store(config).await {
        Ok(store) => {
            if let Err(error) = store.sync_now().await {
                format!("unhealthy ({error})")
            } else {
                "enabled".to_string()
            }
        }
        Err(error) => format!("unhealthy ({error})"),
    }
}

async fn memory_index_status(config: &MoaConfig) -> String {
    match FileMemoryStore::from_config(config).await {
        Ok(store) => match store
            .get_index(MemoryScope::Workspace(current_workspace_id()))
            .await
        {
            Ok(index) => format!("healthy ({} chars)", index.len()),
            Err(error) => format!("unhealthy ({error})"),
        },
        Err(error) => format!("unhealthy ({error})"),
    }
}

fn apply_config_update(config: &mut MoaConfig, key: &str, value: &str) -> Result<()> {
    match key {
        "general.default_provider" => config.general.default_provider = value.to_string(),
        "general.default_model" => config.general.default_model = value.to_string(),
        "general.reasoning_effort" => config.general.reasoning_effort = value.to_string(),
        "cloud.enabled" => config.cloud.enabled = parse_bool(value)?,
        "cloud.turso_url" => config.cloud.turso_url = Some(value.to_string()),
        "cloud.memory_dir" => config.cloud.memory_dir = Some(value.to_string()),
        "cloud.turso_sync_interval_secs" => {
            config.cloud.turso_sync_interval_secs =
                value.parse().context("expected integer sync interval")?
        }
        "local.docker_enabled" => config.local.docker_enabled = parse_bool(value)?,
        "local.sandbox_dir" => config.local.sandbox_dir = value.to_string(),
        "local.session_db" | "database.url" => config.database.url = value.to_string(),
        "database.backend" => {
            config.database.backend = parse_database_backend(value)?;
        }
        "database.pool_min" => {
            config.database.pool_min = value.parse().context("expected integer pool size")?
        }
        "database.pool_max" => {
            config.database.pool_max = value.parse().context("expected integer pool size")?
        }
        "database.connect_timeout_secs" => {
            config.database.connect_timeout_secs =
                value.parse().context("expected integer timeout")?
        }
        "local.memory_dir" => config.local.memory_dir = value.to_string(),
        "daemon.auto_connect" => config.daemon.auto_connect = parse_bool(value)?,
        "daemon.socket_path" => config.daemon.socket_path = value.to_string(),
        "observability.enabled" => config.observability.enabled = parse_bool(value)?,
        "observability.otlp_endpoint" => {
            config.observability.otlp_endpoint = Some(value.to_string())
        }
        _ => bail!("unsupported config key: {key}"),
    }

    Ok(())
}

fn parse_database_backend(value: &str) -> Result<DatabaseBackend> {
    match value.trim().to_ascii_lowercase().as_str() {
        "turso" => Ok(DatabaseBackend::Turso),
        "postgres" => Ok(DatabaseBackend::Postgres),
        _ => bail!("expected `turso` or `postgres`, got {value}"),
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

async fn sync_enable_report(mut config: MoaConfig, turso_url: Option<String>) -> Result<String> {
    let sync_url = match turso_url {
        Some(url) => url,
        None => env::var("TURSO_DATABASE_URL")
            .context("missing Turso database URL; pass one explicitly or set TURSO_DATABASE_URL")?,
    };
    let token_env = config
        .cloud
        .turso_auth_token_env
        .clone()
        .unwrap_or_else(|| "TURSO_AUTH_TOKEN".to_string());
    if env::var(&token_env).is_err() {
        bail!(
            "missing Turso auth token; set {} before enabling sync",
            token_env
        );
    }

    config.cloud.enabled = true;
    config.database.backend = DatabaseBackend::Turso;
    config.cloud.turso_url = Some(sync_url.clone());
    let store = create_session_store(&config)
        .await
        .context("opening cloud-synced session store")?;
    store
        .sync_now()
        .await
        .context("performing initial Turso sync")?;
    config.save()?;

    Ok(format!(
        "cloud sync enabled\nurl: {}\nlocal_db: {}\nsync_interval_secs: {}\n",
        sync_url, config.database.url, config.cloud.turso_sync_interval_secs
    ))
}

#[cfg(test)]
mod tests {
    use super::{apply_config_update, parse_bool, parse_database_backend, version_text};
    use moa_core::{DatabaseBackend, MoaConfig};

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
        apply_config_update(&mut config, "cloud.turso_sync_interval_secs", "5")
            .expect("update sync interval");
        assert_eq!(config.cloud.turso_sync_interval_secs, 5);
    }

    #[test]
    fn parse_bool_accepts_common_values() {
        assert!(parse_bool("yes").expect("bool"));
        assert!(!parse_bool("0").expect("bool"));
    }

    #[test]
    fn parse_database_backend_accepts_supported_values() {
        assert_eq!(
            parse_database_backend("turso").expect("backend"),
            DatabaseBackend::Turso
        );
        assert_eq!(
            parse_database_backend("postgres").expect("backend"),
            DatabaseBackend::Postgres
        );
    }
}

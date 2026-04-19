//! CLI entry point for the MOA load-test harness.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use moa_loadtest::{
    LoadMode, LoadTarget, LoadTestOptions, OutputFormat, SessionProfileKind, render_human_report,
    render_json_report, run_loadtest,
};

/// Runs a synthetic MOA workload against the local orchestrator or daemon.
#[derive(Debug, Parser)]
#[command(name = "moa-loadtest", about = "MOA multi-turn workload generator")]
struct Cli {
    /// Infrastructure mode: `mock` uses ScriptedProvider, `live` uses a real provider.
    #[arg(long, value_enum, default_value_t = LoadMode::Mock)]
    mode: LoadMode,

    /// Target backend: `local` runs in-process, `daemon` drives a running MOA daemon.
    #[arg(long, value_enum, default_value_t = LoadTarget::Local)]
    target: LoadTarget,

    /// Number of concurrent sessions to simulate. `--scale` is an alias.
    #[arg(long, alias = "scale")]
    sessions: Option<usize>,

    /// Session profile family to generate.
    #[arg(long, value_enum, default_value_t = SessionProfileKind::Short)]
    profile: SessionProfileKind,

    /// Delay in milliseconds between turns inside one session.
    #[arg(long, default_value_t = 0)]
    inter_message_delay_ms: u64,

    /// Per-turn timeout in seconds.
    #[arg(long, default_value_t = 60)]
    turn_timeout_seconds: u64,

    /// Final output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    output: OutputFormat,

    /// Optional model override for local live runs.
    #[arg(long)]
    model: Option<String>,

    /// Optional explicit MOA config path.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Optional explicit workspace root for local runs.
    #[arg(long)]
    workspace_root: Option<PathBuf>,

    /// Optional daemon socket override for daemon target runs.
    #[arg(long)]
    daemon_socket: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let sessions = cli.sessions.unwrap_or(match cli.mode {
        LoadMode::Mock => 100,
        LoadMode::Live => 5,
    });
    let options = LoadTestOptions {
        mode: cli.mode,
        target: cli.target,
        sessions,
        profile: cli.profile,
        inter_message_delay: Duration::from_millis(cli.inter_message_delay_ms),
        turn_timeout: Duration::from_secs(cli.turn_timeout_seconds),
        output: cli.output,
        model: cli.model,
        config_path: cli.config,
        workspace_root: cli.workspace_root,
        daemon_socket: cli.daemon_socket,
    };

    let report = run_loadtest(options.clone()).await?;
    let rendered = match options.output {
        OutputFormat::Human => render_human_report(&report),
        OutputFormat::Json => render_json_report(&report)?,
    };
    println!("{rendered}");

    if report.sessions_failed > 0 {
        std::process::exit(1);
    }

    Ok(())
}

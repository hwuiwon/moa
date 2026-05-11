//! CLI entry point for the MOA load-test harness.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use moa_loadtest::{
    LoadMode, LoadTestOptions, OutputFormat, SessionProfileKind, render_human_report,
    render_json_report, run_loadtest,
};

/// Runs a synthetic MOA workload against a Restate-backed orchestrator.
#[derive(Debug, Parser)]
#[command(name = "moa-loadtest", about = "MOA multi-turn workload generator")]
struct Cli {
    /// Infrastructure mode. `mock` expects the orchestrator to run with MOA_PROVIDERS_OVERRIDE.
    #[arg(long, value_enum, default_value_t = LoadMode::Mock)]
    mode: LoadMode,

    /// Restate ingress endpoint fronting `moa-orchestrator`.
    #[arg(long, default_value = "http://localhost:10010")]
    endpoint: String,

    /// Number of concurrent sessions to simulate.
    #[arg(long)]
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

    /// Optional model override for turn requests.
    #[arg(long)]
    model: Option<String>,

    /// Optional explicit MOA config path.
    #[arg(long)]
    config: Option<PathBuf>,
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
        endpoint: cli.endpoint,
        sessions,
        profile: cli.profile,
        inter_message_delay: Duration::from_millis(cli.inter_message_delay_ms),
        turn_timeout: Duration::from_secs(cli.turn_timeout_seconds),
        output: cli.output,
        model: cli.model,
        config_path: cli.config,
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

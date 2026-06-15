//! Binary entry point for the MOA load-test harness.

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
struct Args {
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

    /// Optional global target rate for starting turns.
    #[arg(long)]
    target_qps: Option<u32>,

    /// Per-turn timeout in seconds.
    #[arg(long, default_value_t = 60)]
    turn_timeout_seconds: u64,

    /// Final output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    output: OutputFormat,

    /// Optional model override for turn requests.
    #[arg(long)]
    model: Option<String>,

    /// Optional Prometheus metrics endpoint for per-step latency collection.
    #[arg(long)]
    metrics_endpoint: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let sessions = args.sessions.unwrap_or(match args.mode {
        LoadMode::Mock => 100,
        LoadMode::Live => 5,
    });
    let options = LoadTestOptions {
        mode: args.mode,
        endpoint: args.endpoint,
        sessions,
        profile: args.profile,
        inter_message_delay: Duration::from_millis(args.inter_message_delay_ms),
        target_qps: args.target_qps,
        turn_timeout: Duration::from_secs(args.turn_timeout_seconds),
        output: args.output,
        model: args.model,
        metrics_endpoint: args.metrics_endpoint,
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

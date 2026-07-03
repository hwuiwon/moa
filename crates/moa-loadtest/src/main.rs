//! Binary entry point for the MOA load-test harness.

use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use moa_loadtest::{
    ArrivalProcess, LoadMode, LoadShape, LoadTestOptions, OutputFormat, SessionProfileKind,
    render_human_report, render_json_report, run_loadtest,
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

    /// Optional moa-edge endpoint. When set, turns run through the production
    /// edge SSE path (contact tokens + API keys) instead of trusted headers.
    #[arg(long)]
    edge_endpoint: Option<String>,

    /// Concurrent session pool size.
    #[arg(long)]
    sessions: Option<usize>,

    /// Number of synthetic tenants in the caller pool.
    #[arg(long, default_value_t = 4)]
    tenants: usize,

    /// Identities created per tenant.
    #[arg(long, default_value_t = 2)]
    identities_per_tenant: usize,

    /// Session profile family to generate.
    #[arg(long, value_enum, default_value_t = SessionProfileKind::Short)]
    profile: SessionProfileKind,

    /// Think time in milliseconds before a session takes its next turn.
    #[arg(long, default_value_t = 0)]
    think_time_ms: u64,

    /// Offered turn-start rate in turns/second (open loop). Defaults per mode.
    #[arg(long)]
    rate: Option<f64>,

    /// Offered-rate shape over the window.
    #[arg(long, value_enum, default_value_t = LoadShape::Steady)]
    shape: LoadShape,

    /// Target rate for ramp/stress shapes.
    #[arg(long)]
    rate_end: Option<f64>,

    /// Burst multiplier for the spike shape.
    #[arg(long, default_value_t = 10.0)]
    spike_factor: f64,

    /// Inter-arrival process for the schedule.
    #[arg(long, value_enum, default_value_t = ArrivalProcess::Constant)]
    arrival: ArrivalProcess,

    /// Load window duration.
    #[arg(long, value_parser = humantime::parse_duration, default_value = "60s")]
    duration: Duration,

    /// Warmup prefix excluded from aggregate percentiles.
    #[arg(long, value_parser = humantime::parse_duration)]
    warmup: Option<Duration>,

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

    /// RNG seed for schedules, tenant sampling, and plan generation.
    #[arg(long, default_value_t = 42)]
    seed: u64,
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
        edge_endpoint: args.edge_endpoint,
        sessions,
        tenants: args.tenants,
        identities_per_tenant: args.identities_per_tenant,
        profile: args.profile,
        think_time: Duration::from_millis(args.think_time_ms),
        rate: args.rate.unwrap_or_else(|| args.mode.default_rate()),
        shape: args.shape,
        rate_end: args.rate_end,
        spike_factor: args.spike_factor,
        arrival: args.arrival,
        duration: args.duration,
        warmup: args.warmup,
        turn_timeout: Duration::from_secs(args.turn_timeout_seconds),
        output: args.output,
        model: args.model,
        metrics_endpoint: args.metrics_endpoint,
        seed: args.seed,
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

//! Binary entry point for the graph-memory retrieval performance gate.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use moa_loadtest::scenarios::retrieval::PerfGateConfig;

/// Graph-memory retrieval performance gate.
#[derive(Parser, Debug)]
#[command(about = "Graph-memory retrieval performance gate")]
struct Args {
    /// Number of tenant workspaces to seed and query.
    #[arg(long, default_value_t = 10)]
    workspaces: usize,
    /// Number of facts to seed per workspace.
    #[arg(long, default_value_t = 1000)]
    facts_per_workspace: usize,
    /// Target query rate.
    #[arg(long, default_value_t = 100)]
    qps: u32,
    /// Load window duration.
    #[arg(long, value_parser = humantime::parse_duration, default_value = "5m")]
    duration: Duration,
    /// Hard P95 latency budget in milliseconds.
    #[arg(long, default_value_t = 80)]
    p95_budget_ms: u64,
    /// Soft P99 latency target in milliseconds.
    #[arg(long, default_value_t = 200)]
    p99_soft_target_ms: u64,
    /// Minimum cache hit rate for the repeated-query slice.
    #[arg(long, default_value_t = 0.70)]
    cache_hit_floor: f64,
    /// Prometheus textfile output path.
    #[arg(long, default_value = "target/perf-gate/snapshot.prom")]
    prom_out: PathBuf,
}

impl From<Args> for PerfGateConfig {
    fn from(args: Args) -> Self {
        Self {
            workspaces: args.workspaces,
            facts_per_workspace: args.facts_per_workspace,
            qps: args.qps,
            duration: args.duration,
            p95_budget_ms: args.p95_budget_ms,
            p99_soft_target_ms: args.p99_soft_target_ms,
            cache_hit_floor: args.cache_hit_floor,
            prom_out: args.prom_out,
        }
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    if cfg!(debug_assertions) {
        panic!("perf_gate must be built in --release mode");
    }

    moa_loadtest::scenarios::retrieval::run_perf_gate(Args::parse().into()).await
}

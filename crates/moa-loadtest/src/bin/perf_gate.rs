//! Binary entry point for the graph-memory retrieval performance gate.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, ValueEnum};
use moa_loadtest::scenarios::{mock_smoke::MockSmokeConfig, retrieval::PerfGateConfig};

/// Perf gate profile registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Profile {
    /// Graph-memory retrieval gate backed by Postgres, AGE, pgvector, and Cohere.
    Retrieval,
    /// Short mock loadtest smoke profile with no real LLM calls.
    MockShort,
}

/// Graph-memory retrieval performance gate.
#[derive(Parser, Debug)]
#[command(about = "Graph-memory retrieval performance gate")]
struct Args {
    /// Perf gate profile to run.
    #[arg(long, value_enum, default_value_t = Profile::Retrieval)]
    profile: Profile,
    /// Number of tenant workspaces to seed and query.
    #[arg(long, default_value_t = 10)]
    workspaces: usize,
    /// Number of concurrent virtual users for mock profiles.
    #[arg(long)]
    vus: Option<usize>,
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
    /// Hard P95 latency budget in milliseconds for profile-style gates.
    #[arg(long)]
    max_p95_ms: Option<u64>,
    /// Soft P99 latency target in milliseconds.
    #[arg(long, default_value_t = 200)]
    p99_soft_target_ms: u64,
    /// Minimum cache hit rate for the repeated-query slice.
    #[arg(long, default_value_t = 0.70)]
    cache_hit_floor: f64,
    /// Maximum allowed error rate for mock profiles.
    #[arg(long)]
    max_error_rate: Option<f64>,
    /// Prometheus textfile output path.
    #[arg(long, default_value = "target/perf-gate/snapshot.prom")]
    prom_out: PathBuf,
    /// Restate ingress endpoint fronting `moa-orchestrator`.
    #[arg(long, default_value = "http://localhost:10010")]
    endpoint: String,

    /// Optional explicit MOA config path for profile execution.
    #[arg(long)]
    config: Option<PathBuf>,
}

impl Args {
    fn retrieval_config(&self) -> PerfGateConfig {
        PerfGateConfig {
            workspaces: self.workspaces,
            facts_per_workspace: self.facts_per_workspace,
            qps: self.qps,
            duration: self.duration,
            p95_budget_ms: self.max_p95_ms.unwrap_or(self.p95_budget_ms),
            p99_soft_target_ms: self.p99_soft_target_ms,
            cache_hit_floor: self.cache_hit_floor,
            prom_out: self.prom_out.clone(),
        }
    }

    fn mock_short_config(&self) -> MockSmokeConfig {
        MockSmokeConfig {
            virtual_users: self.vus.unwrap_or(5),
            duration: self.duration,
            max_p95_ms: self.max_p95_ms.unwrap_or(5_000),
            max_error_rate: self.max_error_rate.unwrap_or(0.01),
            prom_out: self.prom_out.clone(),
            endpoint: self.endpoint.clone(),
            config_path: self.config.clone(),
            ..Default::default()
        }
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    if cfg!(debug_assertions) && args.profile == Profile::Retrieval {
        panic!("perf_gate must be built in --release mode");
    }

    match args.profile {
        Profile::Retrieval => {
            moa_loadtest::scenarios::retrieval::run_perf_gate(args.retrieval_config()).await
        }
        Profile::MockShort => {
            moa_loadtest::scenarios::mock_smoke::run_mock_smoke_gate(args.mock_short_config()).await
        }
    }
}

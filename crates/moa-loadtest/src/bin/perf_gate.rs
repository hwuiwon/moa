//! Binary entry point for MOA performance gates.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, ValueEnum};
use moa_loadtest::scenarios::{mock_smoke::MockSmokeConfig, retrieval::PerfGateConfig};

const RETRIEVAL_DEFAULT_TENANTS: usize = 10;
const RETRIEVAL_DEFAULT_FACTS_PER_TENANT: usize = 1_000;
const RETRIEVAL_DEFAULT_QPS: u32 = 100;
const RETRIEVAL_DEFAULT_DURATION: Duration = Duration::from_secs(5 * 60);
const RETRIEVAL_DEFAULT_P95_BUDGET_MS: u64 = 80;
const RETRIEVAL_DEFAULT_P99_SOFT_TARGET_MS: u64 = 200;
const RETRIEVAL_DEFAULT_CACHE_HIT_FLOOR: f64 = 0.70;

const RETRIEVAL_SMOKE_DEFAULT_TENANTS: usize = 2;
const RETRIEVAL_SMOKE_DEFAULT_FACTS_PER_TENANT: usize = 50;
const RETRIEVAL_SMOKE_DEFAULT_QPS: u32 = 5;
const RETRIEVAL_SMOKE_DEFAULT_DURATION: Duration = Duration::from_secs(15);
const RETRIEVAL_SMOKE_DEFAULT_P95_BUDGET_MS: u64 = 1_000;
const RETRIEVAL_SMOKE_DEFAULT_P99_SOFT_TARGET_MS: u64 = 2_000;
const RETRIEVAL_SMOKE_DEFAULT_CACHE_HIT_FLOOR: f64 = 0.50;

#[derive(Clone, Copy)]
struct RetrievalDefaults {
    tenants: usize,
    facts_per_tenant: usize,
    qps: u32,
    duration: Duration,
    p95_budget_ms: u64,
    p99_soft_target_ms: u64,
    cache_hit_floor: f64,
    require_hardware_floor: bool,
}

/// Perf gate profile registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Profile {
    /// Graph-memory retrieval gate backed by Postgres, relational graph tables, pgvector, and Cohere.
    Retrieval,
    /// Developer retrieval smoke that keeps correctness gates but skips the strict hardware floor.
    RetrievalSmoke,
    /// Short mock loadtest smoke profile with no real LLM calls.
    MockShort,
}

impl Profile {
    fn retrieval_defaults(self) -> RetrievalDefaults {
        match self {
            Self::Retrieval => RetrievalDefaults {
                tenants: RETRIEVAL_DEFAULT_TENANTS,
                facts_per_tenant: RETRIEVAL_DEFAULT_FACTS_PER_TENANT,
                qps: RETRIEVAL_DEFAULT_QPS,
                duration: RETRIEVAL_DEFAULT_DURATION,
                p95_budget_ms: RETRIEVAL_DEFAULT_P95_BUDGET_MS,
                p99_soft_target_ms: RETRIEVAL_DEFAULT_P99_SOFT_TARGET_MS,
                cache_hit_floor: RETRIEVAL_DEFAULT_CACHE_HIT_FLOOR,
                require_hardware_floor: true,
            },
            Self::RetrievalSmoke | Self::MockShort => RetrievalDefaults {
                tenants: RETRIEVAL_SMOKE_DEFAULT_TENANTS,
                facts_per_tenant: RETRIEVAL_SMOKE_DEFAULT_FACTS_PER_TENANT,
                qps: RETRIEVAL_SMOKE_DEFAULT_QPS,
                duration: RETRIEVAL_SMOKE_DEFAULT_DURATION,
                p95_budget_ms: RETRIEVAL_SMOKE_DEFAULT_P95_BUDGET_MS,
                p99_soft_target_ms: RETRIEVAL_SMOKE_DEFAULT_P99_SOFT_TARGET_MS,
                cache_hit_floor: RETRIEVAL_SMOKE_DEFAULT_CACHE_HIT_FLOOR,
                require_hardware_floor: false,
            },
        }
    }
}

/// MOA performance gate.
#[derive(Parser, Debug)]
#[command(about = "MOA performance gate")]
struct Args {
    /// Perf gate profile to run.
    #[arg(long, value_enum, default_value_t = Profile::Retrieval)]
    profile: Profile,
    /// Number of tenants to seed and query.
    #[arg(long)]
    tenants: Option<usize>,
    /// Number of concurrent virtual users for mock profiles.
    #[arg(long)]
    vus: Option<usize>,
    /// Number of facts to seed per tenant.
    #[arg(long)]
    facts_per_tenant: Option<usize>,
    /// Target query rate.
    #[arg(long)]
    qps: Option<u32>,
    /// Load window duration.
    #[arg(long, value_parser = humantime::parse_duration)]
    duration: Option<Duration>,
    /// Hard P95 latency budget in milliseconds.
    #[arg(long)]
    p95_budget_ms: Option<u64>,
    /// Hard P95 latency budget in milliseconds for profile-style gates.
    #[arg(long)]
    max_p95_ms: Option<u64>,
    /// Soft P99 latency target in milliseconds.
    #[arg(long)]
    p99_soft_target_ms: Option<u64>,
    /// Minimum cache hit rate for the repeated-query slice.
    #[arg(long)]
    cache_hit_floor: Option<f64>,
    /// Maximum allowed error rate for mock profiles.
    #[arg(long)]
    max_error_rate: Option<f64>,
    /// Prometheus textfile output path.
    #[arg(long, default_value = "target/perf-gate/snapshot.prom")]
    prom_out: PathBuf,
    /// Restate ingress endpoint fronting `moa-orchestrator`.
    #[arg(long, default_value = "http://localhost:10010")]
    endpoint: String,
    /// Optional Prometheus metrics endpoint for mock profile step latency.
    #[arg(long)]
    metrics_endpoint: Option<String>,
}

impl Args {
    fn retrieval_config(&self) -> PerfGateConfig {
        let defaults = self.profile.retrieval_defaults();
        PerfGateConfig {
            tenants: self.tenants.unwrap_or(defaults.tenants),
            facts_per_tenant: self.facts_per_tenant.unwrap_or(defaults.facts_per_tenant),
            qps: self.qps.unwrap_or(defaults.qps),
            duration: self.duration.unwrap_or(defaults.duration),
            p95_budget_ms: self
                .max_p95_ms
                .or(self.p95_budget_ms)
                .unwrap_or(defaults.p95_budget_ms),
            p99_soft_target_ms: self
                .p99_soft_target_ms
                .unwrap_or(defaults.p99_soft_target_ms),
            cache_hit_floor: self.cache_hit_floor.unwrap_or(defaults.cache_hit_floor),
            prom_out: self.prom_out.clone(),
            require_hardware_floor: defaults.require_hardware_floor,
        }
    }

    fn mock_short_config(&self) -> MockSmokeConfig {
        let defaults = MockSmokeConfig::default();
        MockSmokeConfig {
            virtual_users: self.vus.unwrap_or(defaults.virtual_users),
            duration: self.duration.unwrap_or(defaults.duration),
            rate: self.qps.map(f64::from).unwrap_or(defaults.rate),
            max_p95_ms: self.max_p95_ms.unwrap_or(defaults.max_p95_ms),
            max_error_rate: self.max_error_rate.unwrap_or(defaults.max_error_rate),
            prom_out: self.prom_out.clone(),
            endpoint: self.endpoint.clone(),
            metrics_endpoint: self.metrics_endpoint.clone(),
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
        Profile::Retrieval | Profile::RetrievalSmoke => {
            moa_loadtest::scenarios::retrieval::run_perf_gate(args.retrieval_config()).await
        }
        Profile::MockShort => {
            moa_loadtest::scenarios::mock_smoke::run_mock_smoke_gate(args.mock_short_config()).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_short_qps_maps_to_mock_smoke_rate() {
        // Pins: `perf_gate --profile mock-short --qps` controls the open-loop
        // mock smoke offered turn-start rate, not only retrieval QPS.
        let args = Args::parse_from([
            "perf_gate",
            "--profile",
            "mock-short",
            "--qps",
            "7",
            "--duration",
            "20s",
            "--vus",
            "3",
        ]);

        let cfg = args.mock_short_config();

        assert_eq!(cfg.rate, 7.0);
        assert_eq!(cfg.duration, Duration::from_secs(20));
        assert_eq!(cfg.virtual_users, 3);
    }

    #[test]
    fn strict_retrieval_profile_keeps_release_defaults_and_hardware_floor() {
        // Pins: the release retrieval profile remains calibrated to the strict
        // CI hardware floor unless callers explicitly choose retrieval-smoke.
        let args = Args::parse_from(["perf_gate"]);

        let cfg = args.retrieval_config();

        assert_eq!(cfg.tenants, RETRIEVAL_DEFAULT_TENANTS);
        assert_eq!(cfg.facts_per_tenant, RETRIEVAL_DEFAULT_FACTS_PER_TENANT);
        assert_eq!(cfg.qps, RETRIEVAL_DEFAULT_QPS);
        assert_eq!(cfg.duration, RETRIEVAL_DEFAULT_DURATION);
        assert_eq!(cfg.p95_budget_ms, RETRIEVAL_DEFAULT_P95_BUDGET_MS);
        assert_eq!(cfg.p99_soft_target_ms, RETRIEVAL_DEFAULT_P99_SOFT_TARGET_MS);
        assert_eq!(cfg.cache_hit_floor, RETRIEVAL_DEFAULT_CACHE_HIT_FLOOR);
        assert!(cfg.require_hardware_floor);
    }

    #[test]
    fn retrieval_smoke_uses_small_defaults_and_skips_hardware_floor() {
        // Pins: developer retrieval smoke gives a local signal on non-CI
        // hardware without weakening the strict retrieval gate.
        let args = Args::parse_from(["perf_gate", "--profile", "retrieval-smoke"]);

        let cfg = args.retrieval_config();

        assert_eq!(cfg.tenants, RETRIEVAL_SMOKE_DEFAULT_TENANTS);
        assert_eq!(
            cfg.facts_per_tenant,
            RETRIEVAL_SMOKE_DEFAULT_FACTS_PER_TENANT
        );
        assert_eq!(cfg.qps, RETRIEVAL_SMOKE_DEFAULT_QPS);
        assert_eq!(cfg.duration, RETRIEVAL_SMOKE_DEFAULT_DURATION);
        assert_eq!(cfg.p95_budget_ms, RETRIEVAL_SMOKE_DEFAULT_P95_BUDGET_MS);
        assert_eq!(
            cfg.p99_soft_target_ms,
            RETRIEVAL_SMOKE_DEFAULT_P99_SOFT_TARGET_MS
        );
        assert_eq!(cfg.cache_hit_floor, RETRIEVAL_SMOKE_DEFAULT_CACHE_HIT_FLOOR);
        assert!(!cfg.require_hardware_floor);
    }

    #[test]
    fn retrieval_smoke_honors_cli_overrides_without_reenabling_hardware_floor() {
        // Pins: smoke callers can tighten or broaden the local profile while
        // preserving the intentional hardware-floor split.
        let args = Args::parse_from([
            "perf_gate",
            "--profile",
            "retrieval-smoke",
            "--tenants",
            "3",
            "--facts-per-tenant",
            "75",
            "--qps",
            "9",
            "--duration",
            "30s",
            "--max-p95-ms",
            "1500",
            "--p99-soft-target-ms",
            "2500",
            "--cache-hit-floor",
            "0.60",
        ]);

        let cfg = args.retrieval_config();

        assert_eq!(cfg.tenants, 3);
        assert_eq!(cfg.facts_per_tenant, 75);
        assert_eq!(cfg.qps, 9);
        assert_eq!(cfg.duration, Duration::from_secs(30));
        assert_eq!(cfg.p95_budget_ms, 1_500);
        assert_eq!(cfg.p99_soft_target_ms, 2_500);
        assert_eq!(cfg.cache_hit_floor, 0.60);
        assert!(!cfg.require_hardware_floor);
    }
}

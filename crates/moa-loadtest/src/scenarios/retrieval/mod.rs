//! Graph-memory retrieval performance gate scenario.

mod config;
mod isolation;
mod load;
mod reporting;
mod stack;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use futures_util::future::try_join_all;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use moa_brain::{
    planning::{PlannedQuery, Strategy},
    retrieval::{CachedHybridRetriever, HybridRetriever, RetrievalRequest},
};
use moa_core::{TenantId, traits::EmbeddingProvider};
use moa_db::ScopedConn;
use moa_memory_graph::{AgeGraphStore, GraphStore, NodeLabel, NodeWriteIntent, PiiClass};
use moa_memory_types::{MemoryScope, ScopeContext};
use moa_memory_vector::{CohereV4Embedder, PgvectorStore, VECTOR_DIMENSION};
use moa_session::{PostgresSessionStore, testing::cleanup_test_schema};
use rand::{Rng, SeedableRng, rngs::StdRng, seq::SliceRandom};
use secrecy::SecretString;
use serde_json::json;
use sqlx::{PgPool, Row};
use tokio::sync::{Mutex, Semaphore};
use uuid::Uuid;

use config::*;
use isolation::*;
use load::*;
use reporting::*;
use stack::*;

pub use reporting::histogram_percentile;

const QUERY_SEED: u64 = 0xDEAD_BEEF;
const HISTOGRAM_BUCKETS_SECONDS: &[f64] = &[0.005, 0.010, 0.020, 0.040, 0.080, 0.160, 0.320, 0.640];
const LEG_CEILINGS_MS: &[(&str, f64)] = &[
    ("cache_hit", 5.0),
    ("embedder", 30.0),
    ("vector", 15.0),
    ("lexical", 10.0),
    ("graph", 15.0),
    ("rrf_rerank", 10.0),
];

/// Performance gate configuration parsed by the `perf_gate` binary.
#[derive(Debug, Clone)]
pub struct PerfGateConfig {
    /// Number of tenant workspaces to seed and query.
    pub workspaces: usize,
    /// Number of facts to seed per workspace.
    pub facts_per_workspace: usize,
    /// Target query rate.
    pub qps: u32,
    /// Load window duration.
    pub duration: Duration,
    /// Hard P95 latency budget in milliseconds.
    pub p95_budget_ms: u64,
    /// Soft P99 latency target in milliseconds.
    pub p99_soft_target_ms: u64,
    /// Minimum cache hit rate for the repeated-query slice.
    pub cache_hit_floor: f64,
    /// Prometheus textfile output path.
    pub prom_out: PathBuf,
}

/// Runs the graph-memory retrieval performance gate.
pub async fn run_perf_gate(cfg: PerfGateConfig) -> Result<()> {
    let result = async {
        validate_config(&cfg)?;
        validate_hardware_floor()?;
        let metrics = install_metrics_recorder()?;
        run_perf_gate_inner(&cfg, &metrics).await
    }
    .await;
    if let Err(error) = &result {
        let snapshot = format!(
            "# TYPE perf_gate_infrastructure_error gauge\nperf_gate_infrastructure_error 1\n# error: {}\n",
            sanitize_prom_comment(&error.to_string())
        );
        write_snapshot(&cfg.prom_out, &snapshot).await?;
    }
    result
}

async fn run_perf_gate_inner(cfg: &PerfGateConfig, metrics: &PrometheusHandle) -> Result<()> {
    let database_url = std::env::var("MOA_DATABASE_URL")
        .context("MOA_DATABASE_URL is required for perf_gate Postgres/AGE/pgvector access")?;
    let api_key = std::env::var("COHERE_API_KEY")
        .context("COHERE_API_KEY is required for perf_gate embeddings")?;
    let embedder = Arc::new(CohereV4Embedder::new(SecretString::from(api_key)));

    let mut stack = Stack::up(&database_url, embedder).await?;
    let run_result: Result<()> = async {
        stack.seed_workspaces(cfg).await?;
        stack.build_retrievers();
        warm_cache(&stack, cfg).await?;

        let before_load = metrics.render();
        let stop_attacks = Arc::new(AtomicBool::new(false));
        let attack_handle = spawn_cross_tenant_attacks(stack.clone(), stop_attacks.clone());
        let report = drive_load(stack.clone(), cfg).await?;
        stop_attacks.store(true, Ordering::Relaxed);
        let leaks = attack_handle.await.context("RLS attack task panicked")??;
        let after_load = metrics.render();
        let report = report.with_metrics_delta(&before_load, &after_load, cfg);
        let snapshot = render_prometheus(metrics, &report, &leaks);
        write_snapshot(&cfg.prom_out, &snapshot).await?;
        write_stdout(&print_summary_table(&report, &leaks))?;
        enforce_gates(cfg, &report, &leaks)?;
        Ok(())
    }
    .await;
    let cleanup_result = stack.cleanup().await;
    run_result?;
    cleanup_result
}

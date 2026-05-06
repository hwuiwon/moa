# M30 — Performance gate

*Lock the P95 latency budget into CI so retrieval regressions cannot land silently.*

## 1 What this step is about

The graph-primary memory architecture is now functionally complete (M00–M29). M29 proves correctness on a 100-fact corpus; this step proves it stays **fast** under multi-tenant load.

We build a **perf-gate binary** in `moa-loadtest` that:

1. Spins up 10 workspaces × 1000 facts each (10k facts total).
2. Warms the read-time cache (M17).
3. Drives 100 QPS of mixed retrieval queries against the running stack for 5 minutes.
4. Asserts **P95 ≤ 80ms** end-to-end retrieval latency.
5. Asserts **cache hit rate ≥ 70%** on the repeated-query slice.
6. Re-runs the M25 cross-tenant attack suite **concurrently with load** to prove RLS does not leak under pressure.
7. Exits 0 iff every gate passes; non-zero with a Prometheus snapshot otherwise.

This binary is wired into a **nightly CI job**. Any change that breaches the budget fails the build the next morning, not in production.

> ⚠️ Hardware floor matters. Latency is a hardware-relative metric. We document the floor (8 vCPU / 32 GB / NVMe / Postgres co-located on the same VM) in `docs/perf/hardware-floor.md`. The CI runner must match or exceed it; on weaker hardware the gate is allowed to skip with a clear log line, never auto-pass.

This is the **last** prompt in the migration pack. After M30 lands, the graph-primary memory migration is done.

## 2 Files to read

- `crates/moa-loadtest/Cargo.toml` and existing scenarios (see what's already there from earlier load testing)
- `crates/moa-memory/ingest/src/retriever.rs` — the entry point we're stressing
- `crates/moa-memory/ingest/src/cache.rs` from M17
- `crates/moa-eval/tests/golden_e2e.rs` from M29 — borrow the fixture-loader pattern
- `crates/moa-security/tests/cross_tenant.rs` from M25 — borrow the attack suite, run it as a background task
- `docs/runbook/observability.md` — confirm the metric names we'll be reading

## 3 Goal

A reproducible, hardware-pinned, CI-runnable performance gate that:

- Runs locally via `cargo run -p moa-loadtest --release --bin perf_gate -- --workspaces 10 --qps 100 --duration 5m --p95-budget-ms 80`.
- Prints a human-readable summary table on stdout.
- Writes a Prometheus textfile snapshot to `target/perf-gate/snapshot.prom` regardless of pass/fail.
- Returns exit code 0 (pass) or 2 (gate breached) or 1 (infrastructure error).
- Is invoked by a nightly GitHub Actions workflow that uploads the snapshot as a build artifact.

## 4 Rules

1. **Release mode only.** Debug builds are 5–20× slower; gating against them is meaningless. The binary must `panic!` on startup if `cfg!(debug_assertions)`.
2. **Single-binary.** No external load-gen tools (k6, vegeta). Everything in one Rust binary so it's pinned to the workspace toolchain.
3. **No mocked retriever.** The gate exercises the real `Retriever` from M15, the real cache from M17, the real Postgres+AGE+pgvector. Embedder calls go through the real provider in CI (cost is negligible at 10k facts × cached embeddings).
4. **Deterministic seed.** Query mix and fixture generation use a fixed RNG seed. Two consecutive runs must produce P95 within ±10% of each other on the same hardware.
5. **Histograms, not averages.** All latency metrics use `metrics-rs` histograms with explicit buckets `[5ms, 10ms, 20ms, 40ms, 80ms, 160ms, 320ms, 640ms]`. Reporting averages is forbidden.
6. **Concurrent attack suite.** The M25 attacks run on a separate tokio task throughout the load run. Any RLS leak is a hard fail regardless of latency.
7. **No silent skip.** If a precondition is missing (Postgres unreachable, embedder unauthenticated, hardware below floor), the binary exits 1 with a clear message. It never exits 0 without having actually run the gate.

## 5 Tasks

### 5a Hardware floor doc

Create `docs/perf/hardware-floor.md`:

```markdown
# Hardware floor for `perf_gate`

The performance gate (M30) is calibrated against this configuration. Running on
weaker hardware will cause spurious failures; running on stronger hardware will
cause the gate to be too lax and miss regressions.

| Component       | Floor                                                |
| --------------- | ---------------------------------------------------- |
| CPU             | 8 vCPU, x86_64, AVX2 supported                       |
| Memory          | 32 GB                                                |
| Disk            | NVMe SSD, ≥ 500 MB/s sustained random read           |
| Postgres        | 17.6+, co-located on the same VM, shared_buffers=8GB |
| Network         | Embedder reachable in ≤ 50ms RTT P50                 |
| Tokio runtime   | Multi-thread, default worker count                   |

CI nightly runs on `ubuntu-latest` 8-core large runner which matches this floor
(see `.github/workflows/perf-gate.yml`).

If you are running this locally on a laptop, expect P95 to be 1.5–3× higher than
CI; treat local results as directional only.
```

### 5b Binary scaffold

Create `crates/moa-loadtest/src/bin/perf_gate.rs`:

```rust
use anyhow::{bail, Context, Result};
use clap::Parser;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(about = "Graph-memory retrieval performance gate")]
struct Args {
    #[arg(long, default_value_t = 10)]
    workspaces: usize,
    #[arg(long, default_value_t = 1000)]
    facts_per_workspace: usize,
    #[arg(long, default_value_t = 100)]
    qps: u32,
    #[arg(long, value_parser = humantime::parse_duration, default_value = "5m")]
    duration: Duration,
    #[arg(long, default_value_t = 80)]
    p95_budget_ms: u64,
    #[arg(long, default_value_t = 200)]
    p99_soft_target_ms: u64,
    #[arg(long, default_value_t = 0.70)]
    cache_hit_floor: f64,
    #[arg(long, default_value = "target/perf-gate/snapshot.prom")]
    prom_out: String,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    if cfg!(debug_assertions) {
        bail!("perf_gate must be built in --release mode");
    }
    let args = Args::parse();
    moa_loadtest::scenarios::retrieval::run_perf_gate(args.into()).await
}
```

### 5c Scenario module

Create `crates/moa-loadtest/src/scenarios/retrieval.rs` (~600 lines). Outline:

```rust
pub struct PerfGateConfig {
    pub workspaces: usize,
    pub facts_per_workspace: usize,
    pub qps: u32,
    pub duration: Duration,
    pub p95_budget_ms: u64,
    pub p99_soft_target_ms: u64,
    pub cache_hit_floor: f64,
    pub prom_out: PathBuf,
}

pub async fn run_perf_gate(cfg: PerfGateConfig) -> Result<()> {
    install_metrics_recorder();
    let stack = bring_up_stack(&cfg).await?;        // Postgres pool, embedder, retriever
    seed_workspaces(&stack, &cfg).await?;           // 10 × 1000 facts via real ingest path
    warm_cache(&stack, &cfg).await?;                // run query mix once, ignore latencies
    let attack_handle = spawn_cross_tenant_attacks(&stack); // M25 suite, runs throughout
    let report = drive_load(&stack, &cfg).await?;
    let leaks = attack_handle.await??;              // join after load completes
    let prom_snapshot = render_prometheus();
    std::fs::create_dir_all(cfg.prom_out.parent().unwrap())?;
    std::fs::write(&cfg.prom_out, &prom_snapshot)?;
    print_summary_table(&report, &leaks);
    enforce_gates(&cfg, &report, &leaks)?;          // exits process on breach
    Ok(())
}
```

### 5d Query mix

70/20/10 split, fixed seed, deterministic generator:

```rust
fn build_query_mix(seed: u64, total: usize) -> Vec<RetrievalQuery> {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut out = Vec::with_capacity(total);
    let repeated_pool: Vec<_> = (0..50).map(|i| canonical_query(&mut rng, i)).collect();
    for _ in 0..(total * 70 / 100) {
        out.push(repeated_pool.choose(&mut rng).unwrap().clone());
    }
    for _ in 0..(total * 20 / 100) {
        let base = repeated_pool.choose(&mut rng).unwrap();
        out.push(paraphrase(base, &mut rng));
    }
    for _ in 0..(total * 10 / 100) {
        out.push(novel_query(&mut rng));
    }
    out.shuffle(&mut rng);
    out
}
```

The 70% repeated slice is what the cache-hit-rate gate measures against.

### 5e Load driver with rate limiting

Use a `tokio::time::interval` keyed off `cfg.qps`, and a `Semaphore` to bound in-flight concurrency at `qps × P95_budget × 2` so a slow run doesn't cause unbounded queueing:

```rust
async fn drive_load(stack: &Stack, cfg: &PerfGateConfig) -> Result<LoadReport> {
    let queries = build_query_mix(0xDEADBEEF, (cfg.qps as usize) * cfg.duration.as_secs() as usize);
    let mut tick = tokio::time::interval(Duration::from_micros(1_000_000 / cfg.qps as u64));
    let sem = Arc::new(Semaphore::new((cfg.qps as usize) * 2));
    let mut joins = Vec::with_capacity(queries.len());
    let started = Instant::now();
    for q in queries {
        if started.elapsed() >= cfg.duration { break; }
        tick.tick().await;
        let permit = sem.clone().acquire_owned().await?;
        let stack = stack.clone();
        joins.push(tokio::spawn(async move {
            let t0 = Instant::now();
            let res = stack.retriever.retrieve(&q).await;
            let elapsed = t0.elapsed();
            metrics::histogram!("perf_gate_retrieval_seconds").record(elapsed.as_secs_f64());
            drop(permit);
            (res, elapsed, q.is_repeated)
        }));
    }
    let outcomes = futures::future::try_join_all(joins).await?;
    Ok(LoadReport::from_outcomes(outcomes))
}
```

### 5f Latency breakdown assertions

Each leg of retrieval emits its own histogram (already wired in M15/M17). The gate enforces a **per-leg ceiling** to catch regressions that hide inside a healthy total:

| Leg                | P95 ceiling |
| ------------------ | ----------- |
| Cache hit fast-path | 5 ms       |
| Embedder call       | 30 ms      |
| Vector leg          | 15 ms      |
| Lexical leg         | 10 ms      |
| Graph leg           | 15 ms      |
| RRF + rerank        | 10 ms      |

Sum of per-leg P95s is allowed to exceed total P95 (legs run partly in parallel); we only assert each individually.

### 5g Gate enforcement

```rust
fn enforce_gates(cfg: &PerfGateConfig, r: &LoadReport, leaks: &LeakReport) -> Result<()> {
    let mut breaches = vec![];
    if r.p95_ms > cfg.p95_budget_ms as f64 {
        breaches.push(format!("P95 {} ms > budget {} ms", r.p95_ms, cfg.p95_budget_ms));
    }
    if r.cache_hit_rate < cfg.cache_hit_floor {
        breaches.push(format!("cache hit {:.2} < floor {:.2}", r.cache_hit_rate, cfg.cache_hit_floor));
    }
    if leaks.count > 0 {
        breaches.push(format!("RLS leaks observed: {}", leaks.count));
    }
    for (leg, p95, ceil) in &r.leg_breaches() {
        breaches.push(format!("leg {} P95 {} ms > {} ms", leg, p95, ceil));
    }
    if r.p99_ms > cfg.p99_soft_target_ms as f64 {
        eprintln!("⚠️  P99 {} ms exceeds soft target {} ms (warning, not failure)",
                  r.p99_ms, cfg.p99_soft_target_ms);
    }
    if breaches.is_empty() {
        eprintln!("✅ all gates green");
        Ok(())
    } else {
        for b in &breaches { eprintln!("❌ {}", b); }
        std::process::exit(2);
    }
}
```

### 5h Histogram math unit test

```rust
#[test]
fn percentile_is_monotonic_and_within_bucket() {
    let buckets = vec![5.0, 10.0, 20.0, 40.0, 80.0, 160.0, 320.0, 640.0];
    let counts = vec![10, 20, 30, 25, 10, 3, 1, 1];
    let p50 = histogram_percentile(&buckets, &counts, 0.50);
    let p95 = histogram_percentile(&buckets, &counts, 0.95);
    let p99 = histogram_percentile(&buckets, &counts, 0.99);
    assert!(p50 <= p95 && p95 <= p99);
    assert!(p95 <= 80.0 && p95 >= 40.0); // must fall in the 40–80ms bucket
}
```

### 5i CI workflow

Create `.github/workflows/perf-gate.yml`:

```yaml
name: perf-gate
on:
  schedule: [{ cron: '0 7 * * *' }]   # 07:00 UTC nightly
  workflow_dispatch:
jobs:
  gate:
    runs-on: ubuntu-latest-8-core
    timeout-minutes: 30
    services:
      postgres:
        image: ghcr.io/hwuiwon/moa-postgres-age:17.6-age1.7.0
        ports: ['5432:5432']
        env: { POSTGRES_PASSWORD: ci }
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo run -p moa-loadtest --release --bin perf_gate -- --duration 5m
        env:
          DATABASE_URL: postgres://postgres:ci@localhost/postgres
          COHERE_API_KEY: ${{ secrets.COHERE_API_KEY_CI }}
      - uses: actions/upload-artifact@v4
        if: always()
        with:
          name: perf-gate-snapshot
          path: target/perf-gate/snapshot.prom
```

### 5j Baseline document

Create `docs/perf/baseline-2026-04.md` with the first green run's numbers. Future regressions are diffed against this:

```markdown
# Perf gate baseline — 2026-04

First green run on CI hardware floor (8 vCPU / 32 GB / NVMe).

| Metric                  | Value     |
| ----------------------- | --------- |
| Total P50                | 18 ms    |
| Total P95                | 64 ms    |
| Total P99                | 142 ms   |
| Cache hit rate           | 0.78     |
| RLS leaks                | 0        |
| Embedder P95             | 22 ms    |
| Graph leg P95            | 11 ms    |
| Vector leg P95           | 9 ms     |
| Lexical leg P95          | 6 ms     |
| RRF + rerank P95         | 7 ms     |

(Update this file when intentional architectural changes shift the baseline; do
not update it to absorb regressions.)
```

## 6 Deliverables

- `crates/moa-loadtest/src/bin/perf_gate.rs`
- `crates/moa-loadtest/src/scenarios/retrieval.rs`
- `crates/moa-loadtest/src/scenarios/mod.rs` (export `retrieval`)
- `crates/moa-loadtest/Cargo.toml` updated (clap, humantime, metrics, metrics-exporter-prometheus, futures, rand)
- `docs/perf/hardware-floor.md`
- `docs/perf/baseline-2026-04.md`
- `.github/workflows/perf-gate.yml`

## 7 Acceptance criteria

1. `cargo run -p moa-loadtest --release --bin perf_gate -- --duration 5m` exits 0 on a host meeting the hardware floor.
2. P95 retrieval latency ≤ 80 ms over the full 5-minute window.
3. Cache hit rate ≥ 70% on the repeated-query slice.
4. Zero RLS leaks across the M25 attack suite running concurrently with load.
5. Per-leg P95s respect the ceilings in §5f.
6. P99 ≤ 200 ms (soft — warning only, not failure).
7. Prometheus snapshot is always written to `target/perf-gate/snapshot.prom`, including on failure.
8. Two consecutive runs on the same host produce P95 within ±10% of each other.
9. Build fails fast (clear error, exit code 1) if Postgres is unreachable, embedder is unauthenticated, or `cfg!(debug_assertions)` is true.
10. Nightly CI workflow is registered and runs successfully at least once before this prompt is considered complete.

## 8 Tests

The `perf_gate` binary **is** the integration test. Plus one unit test:

```sh
cargo test -p moa-loadtest histogram_math
cargo run -p moa-loadtest --release --bin perf_gate -- --duration 30s   # smoke run
cargo run -p moa-loadtest --release --bin perf_gate -- --duration 5m    # full gate
```

The 30s smoke run is what local development uses; the 5m run is what CI uses.

## 9 Cleanup

None new. M29 provides the golden suite. This step adds files only.

## 10 What's next

**The graph-primary memory migration is complete.**

Future work is tracked outside this prompt pack:

- **MOA-CONNECTORS track** picks up the `Connector` trait scaffold from M20 and adds first-party Slack, Drive, Notion, GitHub connectors with bi-temporal ingestion into the graph.
- **v1.1 backlog** — items deferred during the M00–M30 migration:
  - Swap `tsvector` for ParadeDB `pg_search` BM25 in the lexical leg (requires extension load + reindex).
  - Add a third RRF leg backed by LightRAG-style concept summaries (keep nodes + edges as primary, add a summary index above them).
  - Migrate to Postgres 18 native `uuidv7()` once it ships in stable; remove the application-level UUIDv7 helper.
  - Re-evaluate Turbopuffer vs pgvector sharding once a single workspace passes ~10M facts.

When you start the connectors track, create a fresh `moa/sequence/connectors-pack/` directory and a new C00 overview prompt; do **not** continue the M-series numbering.

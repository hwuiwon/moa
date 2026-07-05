# moa-loadtest

Synthetic load-test harnesses for hosted MOA orchestrator APIs and memory-path validation.

## Running the mock smoke profile locally

The mock smoke profile drives the Restate ingress API with a scripted provider, so it does not make real LLM calls. It prints a short `perf_gate mock-short summary` table, writes a Prometheus textfile snapshot to `target/perf-gate/snapshot.prom`, and exits non-zero if the P95 or error-rate budgets are breached.

```bash
make loadtest-mock
```

The make target starts the local dependencies, bootstraps OpenFGA into
`.env.fga`, restarts the orchestrator with the scripted provider fixture, and
runs `perf_gate --profile mock-short`. It defaults RustFS to host ports
`10090` and `10091` for this path; override them with `MOA_RUSTFS_PORT` and
`MOA_RUSTFS_CONSOLE_PORT` if needed. Tune the local smoke without turning it
into a capacity run through `MOA_LOADTEST_MOCK_DURATION`,
`MOA_LOADTEST_MOCK_VUS`, `MOA_LOADTEST_MOCK_QPS`,
`MOA_LOADTEST_MOCK_MAX_P95_MS`, and `MOA_LOADTEST_MOCK_MAX_ERROR_RATE`.

For an already-running stack, the direct command is:

```bash
set -a
. ./.env.fga
set +a
cargo run --release -p moa-loadtest --bin perf_gate -- \
  --profile mock-short --endpoint http://localhost:10010 \
  --duration 30s --vus 2 --qps 2 \
  --max-p95-ms 5000 --max-error-rate 0.01 \
  --metrics-endpoint http://localhost:10023/metrics
```

The metrics endpoint adds p50, p95, and p99 for the documented turn steps:
snapshot load/write, pipeline compile, LLM call, tool dispatch, and event
persistence.

## Running retrieval perf profiles

The strict retrieval profile is the release gate. It requires the hardware floor
documented in `docs/18-performance.md` and reads `MOA_DATABASE_URL` plus
`MOA_COHERE_API_KEY`.

```bash
cargo run --release -p moa-loadtest --bin perf_gate -- --profile retrieval
```

Use `retrieval-smoke` for developer hardware or quick RAG/retrieval checks. It
keeps retrieval correctness, cache-hit, latency, and RLS gates, but uses smaller
defaults and skips the strict AVX2/CI hardware floor.

```bash
cargo run --release -p moa-loadtest --bin perf_gate -- \
  --profile retrieval-smoke --tenants 2 --facts-per-tenant 50 \
  --qps 5 --duration 15s --max-p95-ms 1000 \
  --p99-soft-target-ms 2000 --cache-hit-floor 0.50 \
  --prom-out target/perf-gate/retrieval-smoke.prom
```

The ignored E2E check for this report shape is:

```bash
MOA_RUN_LOADTEST_REMOTE_SMOKE=1 \
MOA_RESTATE_INGRESS_URL=http://localhost:10010 \
MOA_LOADTEST_METRICS_ENDPOINT=http://localhost:9090/metrics \
cargo test -p moa-loadtest --test mock_loadtest_service_e2e mock_short_profile_reports_runtime_step_latency -- --ignored
```

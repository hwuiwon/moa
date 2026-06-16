# moa-loadtest

Synthetic load-test harnesses for hosted MOA orchestrator APIs and memory-path validation.

## Running the mock smoke profile locally

The mock smoke profile drives the Restate ingress API with a scripted provider, so it does not make real LLM calls. It prints a short `perf_gate mock-short summary` table, writes a Prometheus textfile snapshot to `target/perf-gate/snapshot.prom`, and exits non-zero if the P95 or error-rate budgets are breached.

```bash
cargo run --release -p moa-loadtest --bin perf_gate -- --profile mock-short --duration 30s --max-p95-ms 5000 --max-error-rate 0.01
```

Add `--metrics-endpoint http://localhost:9090/metrics` when the orchestrator
exports Prometheus metrics. The report then includes p50, p95, and p99 for the
documented turn steps: snapshot load/write, pipeline compile, LLM call, tool
dispatch, and event persistence.

The ignored E2E check for this report shape is:

```bash
MOA_RUN_LOADTEST_REMOTE_SMOKE=1 \
MOA_LOADTEST_METRICS_ENDPOINT=http://localhost:9090/metrics \
cargo test -p moa-loadtest --test mock_loadtest_service_e2e mock_short_profile_reports_runtime_step_latency -- --ignored
```

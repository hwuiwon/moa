# moa-loadtest

Synthetic load-test harnesses for hosted MOA orchestrator APIs and memory-path validation.

## Running the mock smoke profile locally

The mock smoke profile drives the Restate ingress API with a scripted provider, so it does not make real LLM calls. It prints a short `perf_gate mock-short summary` table, writes a Prometheus textfile snapshot to `target/perf-gate/snapshot.prom`, and exits non-zero if the P95 or error-rate budgets are breached.

```bash
cargo run --release -p moa-loadtest --bin perf_gate -- --profile mock-short --duration 30s --max-p95-ms 5000 --max-error-rate 0.01
```

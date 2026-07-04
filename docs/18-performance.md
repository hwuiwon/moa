# 18 - Performance

_Performance gate hardware floor and current baseline._

## Hardware Floor

The strict `perf_gate --profile retrieval` release gate is calibrated against
this floor. Weaker hardware causes spurious failures; stronger hardware can hide
regressions.

| Component | Floor |
|---|---|
| CPU | 8 vCPU, x86_64, AVX2 supported |
| Memory | 32 GB |
| Disk | NVMe SSD, >= 500 MB/s sustained random read |
| Postgres | 17.6+, co-located on the same VM, `shared_buffers=8GB` |
| Network | Embedder reachable in <= 50 ms RTT P50 |
| Tokio runtime | Multi-thread, default worker count |

CI nightly runs on `ubuntu-latest-8-core`, which matches this floor. Laptop
results are directional only; expect P95 to be 1.5-3x higher than CI.

`perf_gate --profile retrieval-smoke` is the developer retrieval signal. It uses
smaller defaults (`2` tenants, `50` facts per tenant, `5` QPS, `15s`) and skips
the strict CPU/memory/AVX2 floor while keeping retrieval correctness, RLS,
cache-hit, and broad latency gates. Use it after local RAG/retrieval changes;
use the strict `retrieval` profile for CI, release, and baseline updates.

## Baseline - 2026-04

First green run on CI hardware floor:

| Metric | Value |
|---|---:|
| Total P50 | 18 ms |
| Total P95 | 64 ms |
| Total P99 | 142 ms |
| Cache hit rate | 0.78 |
| RLS leaks | 0 |
| Embedder P95 | 22 ms |
| Graph leg P95 | 11 ms |
| Vector leg P95 | 9 ms |
| Lexical leg P95 | 6 ms |
| RRF + rerank P95 | 7 ms |

Update the baseline only for intentional architectural shifts. Do not update it
to absorb regressions.

## Turn-Path Capacity Baseline (T2) - 2026-07-03

First single-replica capacity characterization with the open-loop harness
(`docs/22-load-and-chaos-testing.md`), on a developer laptop running the full
compose stack plus the generator (directional; below the CI hardware floor).
Workload: mixed profile, scripted providers with realistic pacing
(`scripts/realistic.json`, 0.9-2.0s simulated model latency), 8 tenants,
Poisson think time (mean 1s), text-only turns (the SessionStore-path default
agent context allows zero tool calls, so sandbox execution is excluded).

Steady-rate brackets (90s each, corrected = from intended arrival):

| Offered rate | Achieved | p50 | p95 | p99 | Dispatch-delay p95 |
|---|---:|---:|---:|---:|---:|
| 20 turns/s | 20.7/s | 2.58 s | 8.16 s | 10.0 s | 3 ms |
| 26 turns/s | 26.9/s | 4.32 s | 5.94 s | 6.5 s | 3 ms |
| 32 turns/s | 34.3/s | 7.25 s | 9.27 s | 10.1 s | 3 ms |

Pure-orchestration ceiling (1 ms provider script, short profile, no think
time): ~60 turns/s sustained; at 100 turns/s offered, achieved 58/s with half
the arrivals shed. Under that load `pipeline_compile` p50 rose from 10 ms to
~1 s and `event_persist` p95 to ~2.5 s — context compilation and the Postgres
event append are the first two optimization targets.

Ramp knee (5->120 turns/s over 4 min): latency departs baseline at ~25-30
turns/s offered; beyond it queueing grows linearly while throughput holds to
~34/s. Multi-worker merge (3 generators, 27 turns/s aggregate) reproduces the
same profile with exact merged histograms.

Scale-out arithmetic for the 10k turns/s question: at ~30 turns/s per
laptop-grade replica, 10k turns/s requires ~330 replicas of THIS box — but the
per-replica number on prod hardware (CI floor or better, Postgres not
co-located, no generator contention) must be re-measured via the same T2
procedure before extrapolating; the HPA cap is 50 replicas, so closing the gap
is primarily a per-replica capacity and Postgres write-path question, not a
fan-out question.

Phase A optimization reruns on the same developer-laptop compose profile:

| Run | Achieved | Durable rows/turn | `ProgressUpdate` rows/turn | `event_persist` p50 | `event_persist` p95 | Append tail |
|---|---:|---:|---:|---:|---:|---|
| Transient progress + early outcome | 24.6/s | 2.45 | 0.00 | 2.5 s | 10.0 s | not yet split |
| Direct workflow append, default pool | 23.85/s | 2.39 | 0.00 | 500 ms | 2.5 s | `begin_transaction` p95 2.5 s |
| Direct workflow append, pool 64 | 22.72/s | 2.40 | 0.00 | 250 ms | 2.5 s | `begin_transaction` p95 2.5 s |

Append phase instrumentation showed `lock_session`, `insert_events`, and
`update_session_aggregates` stayed at single-digit-millisecond p95/p99 in the
post-fix runs. A larger orchestrator database pool did not move the remaining
tail, so the next optimization target is DB acquisition/saturation around
`pipeline_compile`, snapshot load/write, and append transaction start rather
than asynchronous progress persistence or aggregate rollups.

## Memory Retrieval Baseline - 2026-06-29

Task 0 of the final low-latency RAG plan refreshed the hermetic PR memory eval
baseline on branch `remove-apache-age` with local compose Postgres at
`postgres://moa_owner:dev@127.0.0.1:10040/moa`.

```bash
cargo run -p xtask -- generate-memory-eval-corpus --profile pr --seed 1 --seed 2 --seed 3 --output target/memory-eval/final-rag-pr
cargo run -p xtask -- run-memory-retrieval-eval --corpus target/memory-eval/final-rag-pr --output target/memory-eval/final-rag-baseline.json
jq '.metrics | {recall_at_4, mrr, ndcg_at_4, zero_recall_rate, p95_retrieval_latency_ms, cross_user_leak_count, pii_unredacted_count, per_leg_recall}' target/memory-eval/final-rag-baseline.json
```

The current report schema stores retrieval metrics under `.metrics`.

| Metric | Value |
|---|---:|
| recall_at_4 | 0.9325 |
| mrr | 0.8743 |
| ndcg_at_4 | 0.8719 |
| zero_recall_rate | 0.0317 |
| p50_retrieval_latency_ms | 682 |
| p95_retrieval_latency_ms | 900 |
| cross_user_leak_count | 0 |
| pii_unredacted_count | 0 |
| per_leg_recall.graph | 0.7778 |
| per_leg_recall.vector | 0.5617 |
| per_leg_recall.lexical | 0.7901 |

Lexical miss analysis was computed by joining `probe_results` leg attribution in
`target/memory-eval/final-rag-baseline.json` with the generated probes and
ledger rows in `target/memory-eval/final-rag-pr`.

| Lexical slice | Expected facts | Lexical misses | Final misses | Notes |
|---|---:|---:|---:|---|
| Exact identifiers | 15 | 4 | 4 | All misses are private-repository exact-memory-id probes for seed 2 users. |
| Product/SKU-like strings | 0 | 0 | 0 | No PR-profile probes or facts cover this slice. |
| Quoted errors | 0 | 0 | 0 | No PR-profile probes or facts cover this slice. |
| Document titles | 0 | 0 | 0 | No PR-profile document-title probes are present. |
| Runbook path proxy | 6 | 0 | 0 | `runbook/...` path-style exact tokens are clean in this run. |
| Structured tokens | 162 | 34 | 13 | Misses are dominated by multi-hop `owned_by` facts; graph/vector usually recover them, but exact-id misses do not recover. |

BM25 gate: proceed only if the next task directly targets the observed exact
identifier and structured-token lexical gaps. Do not justify BM25 with
product/SKU, quoted-error, or document-title claims until a corpus/report
actually contains those probes and shows a miss or latency bottleneck. Do not
implement later final-RAG tasks unless they address an observed gap in this
baseline or in a newer measured baseline.

## Turn-Step Loadtest

`moa-loadtest` can collect p50, p95, and p99 latency for the documented turn
steps when the orchestrator metrics endpoint is available:

```bash
cargo run --release -p moa-loadtest --bin perf_gate -- \
  --profile mock-short \
  --metrics-endpoint http://localhost:9090/metrics
```

Use the step report to choose the next optimization target. Do not tune
`pipeline_compile`, `llm_call`, `tool_dispatch`, or `event_persist` until one of
them dominates the measured run.

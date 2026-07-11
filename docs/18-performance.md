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

## Turn-Path Capacity Rebaseline (T2) - 2026-07-10

Single-replica developer-laptop compose run after removing the foreground
segment-view refresh, lineage event reread, serial bounded progress fan-in, and
the authz poller's explicit claim transaction. The append metric now separates
pool acquisition from transaction start. Command: `make loadtest-capacity`
with the realistic scripted provider, mixed profile, 800 sessions, 8 tenants,
2-second mean think time, and a 5-to-200 turns/s linear ramp over 10 minutes.

This run is directional rather than a strict before/after comparison with the
2026-07-03 ramp: the older ramp ended at 120 turns/s after 4 minutes and used a
1-second mean think time. Both ran on the same developer laptop with Postgres,
Restate, the orchestrator, and the generator co-located, but background host
work was not controlled. Do not attribute the changed knee solely to the code
changes.

| Result | Value |
|---|---:|
| Last stable offered bracket | about 46-49 turns/s |
| Stable completed throughput | about 43-44 turns/s |
| First overloaded offered bracket | about 52 turns/s |
| Whole-run achieved rate | 17.38 turns/s |
| Corrected latency p50 / p95 / p99 | 35.13 s / 61.67 s / 104.01 s |
| Durable rows per completed turn | 2.91 |
| Dropped arrivals / scheduled arrivals | 49,921 / 61,498 |

The knee is visible between adjacent 10-second windows: at about 49 turns/s
offered, 44.1 turns/s completed with corrected p95 4.08 seconds; at about 52
turns/s offered, completions fell to 33.1 turns/s and corrected p95 rose to 7.13
seconds. The whole-run tail includes deliberate overload above the knee and the
timeout-bounded drain, so it is not a steady-state service-level target.

| Append phase | p50 | p95 | p99 |
|---|---:|---:|---:|
| `acquire_connection` | 2.5 ms | 1.0 s | 2.5 s |
| `begin_transaction` | 1.0 ms | 10 ms | 25 ms |
| `commit` | 2.5 ms | 100 ms | 250 ms |

`event_persist` remains the dominant measured turn step at the tail (p95 10
seconds, p99 30 seconds), ahead of `llm_call` and `pipeline_compile` (both p95
5 seconds, p99 10 seconds). Within append, pool acquisition dominates;
transaction start and commit are materially smaller. This rejects event
batching as the next branch for the measured bottleneck.

**Selected next workstream:** fleet admission and database connection capacity.
Define an explicit foreground/background connection budget across replicas,
bound admission before shared pools saturate, and evaluate a prepared-statement-
compatible pooler configuration. Re-run the same T2 shape after that one branch
before considering event batching, progress projection, or context/skill
caching. This single-node result still does not certify 10,000 turns/s.

## Admission and Connection-Budget Rebaseline (T2) - 2026-07-11

The selected admission branch was repeated with the same 5-to-200 turns/s,
10-minute scripted-provider ramp. The orchestrator used five foreground
connections, one independently owned background connection, and a three-second
foreground acquire timeout. The production overlay also enables the runtime-
store-backed global provider concurrency scope and first-byte, idle, and total
stream deadlines; scripted providers bypass those live-provider controls, so
this T2 result measures the database admission branch only.

| Result | 2026-07-10 | 2026-07-11 |
|---|---:|---:|
| Last stable offered bracket | about 46-49 turns/s | about 59 turns/s |
| Stable completed throughput | about 43-44 turns/s | about 50 turns/s |
| First overloaded offered bracket | about 52 turns/s | about 62 turns/s |
| Whole-run achieved rate | 17.38 turns/s | 27.30 turns/s |
| Corrected latency p50 / p95 / p99 | 35.13 s / 61.67 s / 104.01 s | 43.48 s / 75.17 s / 85.72 s |
| Durable rows per completed turn | 2.91 | 2.58 |
| Dropped arrivals / scheduled arrivals | 49,921 / 61,498 | 45,033 / 61,498 |
| Turn timeouts | not recorded in the table | 645 |

At about 59 turns/s offered, 50.3 turns/s completed with corrected p95 4.70
seconds. At about 62 turns/s offered, completions fell to 39.0 turns/s and
corrected p95 rose to 7.63 seconds. Whole-run latency remains dominated by the
deliberate overload tail and must not be read as a steady-state SLO.

| Append phase | 2026-07-10 p50 / p95 / p99 | 2026-07-11 p50 / p95 / p99 |
|---|---:|---:|
| `acquire_connection` | 2.5 ms / 1.0 s / 2.5 s | 50 ms / 1.0 s / 2.5 s |
| `begin_transaction` | 1.0 ms / 10 ms / 25 ms | 1.0 ms / 1.0 ms / 5.0 ms |
| `commit` | 2.5 ms / 100 ms / 250 ms | 2.5 ms / 25 ms / 100 ms |

The smaller foreground pool deliberately moves queuing to acquisition while
reducing contention after admission: `BEGIN` and commit tails improved, the
directional knee moved right, and more scheduled work completed. Acquisition
still dominates the append tail, so increasing per-pod pools is not the next
step; it would violate the fleet budget and the previous pool-64 experiment did
not improve capacity. The next required evidence is T3 scale-out with a real
database connection ceiling. PgBouncer remains conditional on deployed-version
and SQLx prepared-statement compatibility. This result does not certify 10,000
turns/s.

## Local Foreground Pool Sweep - 2026-07-11

A controlled local A/B test evaluated whether increasing the orchestrator
foreground pool from 5 to 10 improves the admission-branch result. Each profile
used independently fresh Postgres, Restate, and OpenFGA volumes, the required
Restate `*` virtual-queue rule at concurrency 1000, one background database
connection, and the same realistic scripted-provider workload. The shorter
5-to-100 turns/s ramp ran for five minutes, preserving approximately the same
offered-rate slope through the previously observed knee.

| Result | Pool 5 | Pool 10 |
|---|---:|---:|
| Whole-run achieved rate | 43.92 turns/s | 25.80 turns/s |
| Completed turns | 12,104 | 7,217 |
| Failed sessions / turn timeouts | 0 / 0 | 623 / 623 |
| Dropped arrivals / scheduled arrivals | 3,644 / 15,748 | 7,908 / 15,748 |
| Corrected latency p50 / p95 / p99 | 8.72 s / 50.86 s / 53.87 s | 15.97 s / 56.69 s / 101.78 s |
| Durable rows per completed turn | 2.37 | 2.70 |
| `acquire_connection` p50 / p95 / p99 | 50 ms / 500 ms / 2.5 s | 1 ms / 1.0 s / 2.5 s |
| `begin_transaction` p50 / p95 / p99 | 1 ms / 1 ms / 2.5 ms | 1 ms / 2.5 ms / 10 ms |
| `commit` p50 / p95 / p99 | 2.5 ms / 25 ms / 50 ms | 2.5 ms / 25 ms / 100 ms |

Pool 10 admits more concurrent database work but increases transaction and
commit contention, moves the latency knee left, and completes substantially
less work. This agrees with the older pool-64 experiment, which also failed to
improve throughput. The measured local maximum among the tested production-
relevant profiles remains five foreground connections per orchestrator replica;
do not raise the checked-in value without a different database topology and a
new controlled sweep.

## Memory Retrieval Baseline - 2026-06-29

Task 0 of the final low-latency RAG plan refreshed the hermetic PR memory eval
baseline on branch `remove-apache-age` with local compose Postgres at
`postgres://moa_owner:dev@127.0.0.1:10040/moa`.

```bash
cargo run -p xtask --features eval-tools -- generate-memory-eval-corpus --profile pr --seed 1 --seed 2 --seed 3 --output target/memory-eval/final-rag-pr
cargo run -p xtask --features eval-tools -- run-memory-retrieval-eval --corpus target/memory-eval/final-rag-pr --output target/memory-eval/final-rag-baseline.json
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

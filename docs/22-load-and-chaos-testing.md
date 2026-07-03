# Load & Chaos Testing

This doc defines how MOA measures capacity ("can we survive 10k+ QPS?"),
how it verifies resilience ("can we survive one or more system failures?"),
and how latency percentiles are collected for pipeline optimization.
The harness lives in `crates/moa-loadtest`; chaos scenarios live in
`crates/moa-loadtest/src/scenarios/chaos/`.

## Definitions

**QPS unit.** The headline capacity number is **turn-starts per second at the
edge** (`POST /v1/sessions/{id}/messages`). One turn fans out into roughly
5–10 Restate invocations, multiple Postgres event appends, one or more LLM
calls, and 0–N tool dispatches; edge RPS and Restate invocations/sec are
reported alongside as derived load, never as the headline.

**Layer.** Capacity is certified against the deterministic scaffold —
edge → Restate → orchestrator → brain pipeline → Postgres — using scripted
providers with realistic latency/token shapes (`MOA_PROVIDERS_OVERRIDE`).
Live-provider lanes exist only to calibrate the scripted shapes against
reality, never to carry the capacity claim.

**Measurement discipline.** Load generation is open-loop at a constant
arrival rate (wrk2 model): turn start times are scheduled on a fixed
timeline, latency is measured from the *intended* start, and samples are
recorded to HdrHistograms. Reports include both coordinated-omission
corrected and uncorrected percentiles; the corrected number is the SLO
number. Client-side HDR percentiles are the source of truth for end-to-end
latency; server-side Prometheus histograms (`moa_turn_step_duration_seconds`)
attribute that latency to pipeline steps.

## Capacity model

Per-turn cost bill (measured, not assumed — T2 produces these numbers):

| Resource | Cost per turn | Shared limit |
|---|---|---|
| Restate invocations | ~5–10 | per-tenant scope concurrency (1000 in compose/prod rules) |
| Postgres event appends | 3–8 rows + blob offload >64KiB | orchestrator pool (default 20 conns/replica), single writer |
| Postgres reads | snapshot load + authz + retrieval legs | same pool + edge pool (50) |
| LLM call | 1+ (with retries/failover) | provider concurrency (default unbounded) + RatePacer |
| SSE stream | 1 long-lived HTTP conn | edge conn limits, broadcast channel capacity |

Aggregate capacity ≈ per-replica sustainable turn rate × orchestrator
replicas, until a shared resource saturates. Expected first wall: Postgres
write path (event appends are per-turn and unbatched). The per-tenant scope
cap of 1000 concurrent invocations means a 10k QPS claim is only meaningful
with a multi-tenant workload distribution.

## Tiers

| Tier | Where | Cadence | Question answered |
|---|---|---|---|
| T1 smoke | compose, 1 replica | every PR (`perf_gate --profile mock-short`) | did p95/error-rate regress? |
| T2 capacity | one strong box, compose | nightly (`make loadtest-capacity`) | max sustainable turns/sec per replica + per-turn resource bill |
| T3 scale-out | k8s topology (HPA 2–50) | pre-release / on demand | does capacity scale ≈ linearly with replicas, and what breaks first? |

T3 certifies the 10k+ QPS claim as arithmetic validated by measurement:
`replicas_needed = ceil(10_000 / per_replica_rate)` must be ≤ HPA max, and a
scale-out run at the computed replica count must sustain the target rate
with p99 within budget and zero invariant violations.

## Load shapes

Named scenarios (`moa-loadtest` CLI `--shape`): `steady` (SLO validation at a
fixed rate), `ramp` (find the knee), `spike` (short 10× burst; HPA and queue
drain), `soak` (hours at ~70% capacity; leaks, compaction, partition growth),
`stress` (past the knee; graceful degradation + chaos invariants).

## Chaos methodology

Every experiment is hypothesis-driven and runs **under load**:

1. establish steady state (assert baseline error rate + p95),
2. inject the fault,
3. wait for a recovery deadline,
4. run the invariant checker (`moa-test-support`),
5. report recovery time (time until p99 back under budget and backlog drained).

Fault injection layers:

- **Layer A — deterministic scripts** (PR lane): scripted-provider fault
  fields (429/500/timeouts/mid-stream aborts) drive RateGuard, failover, and
  retry budgets through the real orchestrator; failpoints in storage crates
  fail the Nth call deterministically.
- **Layer B — process/container chaos** (nightly, compose): SIGKILL the
  orchestrator mid-turn, restart Restate/Postgres, stop OpenFGA/Valkey/PII.
- **Layer C — network faults** (nightly/weekly, Toxiproxy overlay): latency,
  jitter, partitions, connection resets on the Postgres/OpenFGA/provider and
  edge→Restate links.

Restate's own consistency is verified upstream by `restatedev/jepsen`; MOA
tests only its contract with Restate. Assertions read Postgres events and
Prometheus metrics — never traces, because Restate replay suppresses spans.

## Invariants (checked after every chaos experiment)

| Invariant | Source of truth |
|---|---|
| Every started turn reaches a terminal outcome; no session stuck past deadline | session meta + events |
| Event log gapless: `sequence_num` monotonic; no dedupe key materialized twice | `session_events` + `session_event_dedupe` |
| Non-idempotent tools executed ≤ once per idempotency key | tool ledger + event log |
| OpenFGA outbox drained post-recovery; dead-letter empty unless expected | authz outbox tables |
| Authz fails closed during FGA outage (denials, never allows) | edge/orchestrator responses |
| PII filter fails closed during PII-service outage | ingestion results |
| No duplicate events after orchestrator kill + Restate replay | dedupe scan |
| Recovery SLO: p99 under budget and backlog drained within deadline | windowed client histograms |

## Metrics wiring

- Orchestrator and edge expose Prometheus at `:9090` (compose hosts
  `10023` and `10001`; enabled via `MOA_METRICS_ENABLED`, default on in
  compose). k8s wires the orchestrator port in
  `k8s/base/20-orchestrator-deployment.yaml`.
- Turn-step attribution: `moa_turn_step_duration_seconds{step=...}` with
  sub-10ms buckets (see `moa-observability/src/runtime_metrics.rs`).
- Tokio runtime gauges require a `tokio_unstable` build
  (`RUSTFLAGS="--cfg tokio_unstable"`); perf images should enable it.
- Baselines live in `docs/18-performance.md` and are updated from T2 runs.

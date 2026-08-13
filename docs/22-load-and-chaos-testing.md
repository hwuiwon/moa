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
attribute that latency to pipeline steps. When a metrics endpoint is supplied,
reports also include a `resource_bill` block from
`moa_session_events_appended_total`: total durable event rows per completed
turn, `ProgressUpdate` rows per turn, and the per-event-type row split. They
also include
`event_append_phase_latency_ms` from
`moa_session_event_append_phase_seconds{phase=...}` so `event_persist` can be
split into bounded append-store phases such as row-lock acquisition, event
insert, aggregate update, and commit wait. Pool connection acquisition and SQL
transaction start are separate phases (`acquire_connection` and
`begin_transaction`) so saturation can be distinguished from PostgreSQL `BEGIN`
latency. Edge-mode reports include
`edge_observation_wait_ms` when the first `response` SSE frame carries a
durable event timestamp, estimating post-persist observation lag that
server-side turn metrics do not see.

## Capacity model

Per-turn cost bill (measured, not assumed — T2 produces these numbers):

| Resource | Cost per turn | Shared limit |
|---|---|---|
| Restate invocations | ~5–10 | Session `TurnAdmission` fleet and per-tenant Valkey leases |
| Postgres event appends | 3–8 rows + blob offload >64KiB | foreground orchestrator pool (production base 20 conns/replica), single writer |
| Postgres reads | snapshot load + authz + retrieval legs | same foreground pool + edge pool (production base 8) |
| Background Postgres work | outbox, analytics, lineage, memory ingestion | independent orchestrator pool (production base 1 conn/replica) |
| LLM call | 1+ (with retries/failover) | provider concurrency (default 16 per provider credential; production uses runtime-store-backed global scope) + process-local RatePacer/RateGuard + stream deadlines |
| SSE stream | 1 long-lived HTTP conn | edge conn limits, broadcast channel capacity |

Aggregate capacity ≈ per-replica sustainable turn rate × orchestrator
replicas, until a shared resource saturates. Expected first wall: Postgres
write path (event appends are per-turn and unbatched). The configured shared
fleet and per-tenant turn limits mean a 10k QPS claim is only meaningful with
a multi-tenant workload distribution and matching Valkey admission capacity.

Production connection admission is explicit. `MOA_DATABASE_MAX_CONNECTIONS`
controls the foreground orchestrator pool,
`MOA_DATABASE_BACKGROUND_MAX_CONNECTIONS` isolates continuous maintenance
work, and `MOA_DATABASE_CONNECT_TIMEOUT_SECONDS` bounds pool acquisition. The
base deployment uses 20 + 1 connections per orchestrator replica. At the HPA
maximum of 50 replicas, the orchestrator reserves 1,050 connections and edge
reserves 24; production must provide a verified database/proxy envelope of at
least 1,200 so migrations, deploy overlap, and operator access retain 126
connections of headroom.

Provider production admission sets `MOA_PROVIDERS_CONCURRENCY_SCOPE=global`.
Streaming requests are bounded independently by
`MOA_PROVIDERS_STREAM_TIMEOUTS_FIRST_BYTE_MS`,
`MOA_PROVIDERS_STREAM_TIMEOUTS_IDLE_MS`, and
`MOA_PROVIDERS_STREAM_TIMEOUTS_TOTAL_MS`. The global concurrency lease TTL must
exceed the total stream deadline so an active generation cannot outlive its
lease under valid configuration.

## Tiers

| Tier | Where | Cadence | Question answered |
|---|---|---|---|
| T1 mock smoke | compose, 1 replica | every PR (`perf_gate --profile mock-short`, locally via `make loadtest-mock`) | did p95/error-rate regress? |
| Retrieval smoke | local Postgres + embedder | after RAG/retrieval changes (`perf_gate --profile retrieval-smoke`) | did retrieval correctness, RLS, cache-hit, or broad latency regress on developer hardware? |
| T2 capacity | one strong box, compose | nightly (`make loadtest-capacity`) | max sustainable turns/sec per replica + per-turn resource bill |
| T3 scale-out | k8s topology (HPA 2–50) | pre-release / on demand | does capacity scale ≈ linearly with replicas, and what breaks first? |

## Long-horizon execution validation

The `long-horizon-execution` nextest profile is the deterministic durability
lane for execution runs whose product horizon is measured in days or weeks. It
uses the production Restate handlers, PostgreSQL repositories and outbox, and
Valkey runtime-cache backend with fixture providers only. Provider credentials
are removed by `scripts/run-clean-e2e.sh --live --long-horizon`; this lane must
never enable a live-provider flag or consume a provider budget.

The suite represents eight logical days with compressed real intervals. One
logical day is two real seconds, so plan-authored `At`/`After` waits still travel
through the production compiler, persisted absolute `due_at`, Restate
`send_after`, database `now()`, and generation-fenced trigger delivery. This is
not a fake clock and tests must assert the persisted due time, delivery order,
and exact generation rather than treating elapsed test time as evidence.

Every implemented user-visible parked status checks the parked-resource
invariant at each input, review, signal, timer, and pause interval:

- exactly one durable `parked_runs` receipt for the parked run;
- zero non-released `active_tasks` capacity receipts for the parked run;
- no active dispatch UID or running attempt retained by a parked task;
- no live sandbox hand/workspace operation retained for storage-only waits;
- no continuing Restate invocation pinned merely to wait for wall time.

`WaitingExternal` deliberately retains one bounded `external_jobs` receipt for
the provider-owned job, but still retains zero `active_tasks`, sandbox hands, or
continuing attempt invocations. Provider progress updates only PostgreSQL and a
sparse due trigger; it does not create a hot controller loop.

The retry-backoff case separately proves a future persisted `ready_at`, no
active task reservation or dispatch during the backoff, and no generation-two
provider call before that timestamp; it does not mislabel retry readiness as a
user-visible parked run.

The 1,000-run common-wake burst also observes the production OTLP
`moa_execution_dispatch_batch_size` and
`moa_execution_oldest_ready_age_seconds` instruments from the bounded
dispatcher callsite, alongside exact database capacity and Restate invocation
counts. SQL queue age alone is diagnostic evidence, not a substitute for
proving that the production metric is wired.

The lane drives `WaitingExternal` through an integration-only catalog tool whose
declared async mode selects a deterministic HTTP provider adapter. That path
asserts pre-start reservation and idempotency-key equality, post-bind terminal
callback deferral until the owning attempt releases capacity, progress callback
deduplication and reconcile rearming, sparse reconciliation while paused, and
unbound-start recovery after total Restate-state loss without replaying the
original task attempt. The built-in tool fails closed if execution bypasses its
declared adapter, and neither the adapter nor its catalog entry exists outside
the provider-override integration lane. Sandbox release coverage also uses a real
sandbox-required hand capability: it observes the exact execution-task
`active_hands` receipt while the task runs, its release during a storage-only
wait, and a downstream task's distinct receipt after resume. The implemented
deterministic matrix covers an accelerated week, deadlines and wait expiry,
pause, an Agent action-review checkpoint and redispatch, idempotent versus
ambiguous watchdog expiry, governed retry, burst admission fairness and
fleet/tenant caps, three real Restate handler deployments draining in order,
and dependency recovery. The Agent review case proves the exact pending effect
is checkpointed after active-task capacity release and that approval creates a
new bounded attempt dispatch; it does not wait on a Restate promise.

Recovery cases include pausing a storage-only timer before its due time: the
timer and task settle once while the run remains parked with no controller
activation, and a generation-fenced resume creates the only activation that
advances the settled graph. Other cases restart the orchestrator and Valkey
repeatedly, stop/start PostgreSQL without replacing its durable volume, replay
late or duplicate input, signal, review, trigger, and outbox deliveries, and
replace Restate with an empty node on the same endpoints. Production
reconciliation re-drives the exact generation-fenced dispatch identity from
PostgreSQL, so cluster replacement cannot duplicate a logical delivery. A
Running non-idempotent
attempt is deliberately excluded from dispatch re-drive; after empty-state
replacement, only its durable watchdog may classify it as `UnknownOutcome`.
Tests use Restate's
deployment and invocation system tables only to observe routing and drain;
PostgreSQL remains the product state source of truth.

Run the deterministic lane with:

```bash
MOA_RUN_LIVE_E2E=1 ./scripts/run-clean-e2e.sh --live --long-horizon
```

The separate 24-hour and seven-day deployment canaries are ignored by default.
They require both the explicit `MOA_RUN_LONG_HORIZON_CANARY=1` gate and
`MOA_LONG_HORIZON_CANARY_WINDOW=24h` or `7d`. They refuse to construct a local
fallback stack: `MOA_DATABASE_URL`, `MOA_RESTATE_INGRESS_URL`, and
`RESTATE_ADMIN_URL` must identify the deployed system. They remain unbilled and
fail closed if any `MOA_RUN_LIVE_*=1` integration flag is present. Run them
without provider credentials as defense in depth:

```bash
env -u MOA_ANTHROPIC_API_KEY -u MOA_OPENAI_API_KEY \
  -u MOA_GOOGLE_API_KEY -u MOA_COHERE_API_KEY \
  -u MOA_ZEROENTROPY_API_KEY -u MOA_FIDELITY_SIMULATOR_API_KEY \
  -u MOA_LLAMAPARSE_API_KEY -u MOA_MERGE_API_KEY -u MOA_NANGO_API_KEY \
  -u MOA_NEON_API_KEY -u MOA_DATABASE_NEON_API_KEY \
  -u MOA_REDUCTO_API_KEY -u MOA_TEST_MCP_DEPLOYMENT_API_KEY \
  -u MOA_TURBOPUFFER_API_KEY -u MOA_UNSTRUCTURED_API_KEY \
  MOA_RUN_LONG_HORIZON_CANARY=1 \
  MOA_LONG_HORIZON_CANARY_WINDOW=24h \
  cargo nextest run -p moa-orchestrator --locked \
    --test long_horizon_execution_canary_live \
    -E 'test(/^deployed_long_horizon_invariants_hold_for_24_hours_live$/)' \
    --run-ignored ignored-only --no-tests fail
```

Use the corresponding exact seven-day test name when
`MOA_LONG_HORIZON_CANARY_WINDOW=7d`. A named canary fails closed when its test
name and configured window differ, so a wrong selection cannot report a
successful soak that sampled nothing.

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

## Runbooks

**T1 mock smoke (PR gate).** `make loadtest-mock` starts compose dependencies,
boots the required Valkey admission backend, bootstraps OpenFGA into `.env.fga`,
and restarts the orchestrator with
`scripts/perf-gate.json`, and runs `cargo run --release -p moa-loadtest --bin
perf_gate -- --profile mock-short`. The target defaults RustFS to host ports
`10090` and `10091` for this local smoke path. Gates: corrected p95, turn error
rate, session failures.

**Retrieval smoke.** `cargo run --release -p moa-loadtest --bin perf_gate -- \
--profile retrieval-smoke --tenants 2 --facts-per-tenant 50 --qps 5 \
--duration 15s --max-p95-ms 1000 --p99-soft-target-ms 2000 \
--cache-hit-floor 0.50`. This profile is for developer machines and skips the
strict AVX2 hardware floor. It uses the local pgvector path unless the test
database explicitly configures a storage partition for Turbopuffer; such cloud
vector runs must set `MOA_TURBOPUFFER_API_KEY` and should fail closed if the
client is missing. The release `retrieval` profile remains strict and is the
only source for baseline updates.

**T2 capacity (nightly).** `make loadtest-capacity` — recreates the
dependencies, bootstraps OpenFGA, then
recreates the orchestrator with `scripts/realistic.json` (real latency/TTFT
pacing, tool loop) and ramps 5→200 turns/s over 10 minutes across 8 tenants.
The capacity target and Kubernetes base use 20 foreground database connections
per orchestrator. To test a comparison profile, run
`MOA_DATABASE_MAX_CONNECTIONS=<n> make loadtest-capacity` and record that
profile with the result. The direct lane writes
`target/perf-gate/capacity-direct.json`; `make loadtest-capacity-edge` writes the
separate production-path `target/perf-gate/capacity-edge.json`. The report
manifest records source revision/state, database pools, state identity, lane,
hardware, and resolved load options. Classify the knee from completed throughput,
corrected latency, dropped arrivals, failures, and queue/utilization metrics;
dispatch-delay p95 is one signal, not the sole criterion. Record the per-replica
sustainable rate, database pool profile, and per-turn step latencies in
`docs/18-performance.md`.

`make loadtest-capacity-direct-append` runs the named-action event-append
variant and writes `target/perf-gate/capacity-direct-append.json`. The report
manifest records the append variant, so it cannot be merged with the default
SessionStore-RPC control accidentally.

`make loadtest-capacity-brackets` runs the pool-5, pool-10, pool-20 ramp
comparisons plus constant pool-20 50/55/60/65 turns/s brackets in randomized
order. Every bracket uses a unique Compose project and therefore fresh Restate
state; the target records the realized order under `target/perf-gate/brackets/`
and stops each stack without deleting its diagnostic volume.

**Soak.** `make loadtest-soak SOAK_RATE=<70% of knee> SOAK_DURATION=8h`;
watch the window series for drift (leaks, compaction pressure, event
partition growth).

**T3 scale-out (multi-worker).** Shard the schedule across worker processes
or hosts — each runs `moa-loadtest ... --seed <distinct> --output json >
worker-N.json` — then merge losslessly: `moa-loadtest --merge worker-*.json`
(reports embed HdrHistograms; merged percentiles are exact). Certify
10k+ QPS by driving `sum(worker rates) >= 10_000 / replicas` per replica
count and confirming merged corrected p99 stays in budget with zero
invariant violations.

**Edge mode.** `make loadtest-edge-keys`, export the printed env, then run
`make loadtest-capacity-edge` — turns
run through the production SSE path with real API keys and contact tokens,
TTFT is measured from the first `response` frame, and
`edge_observation_wait_ms` captures response event timestamp-to-client receipt
lag when available.

**Chaos.** `make chaos-smoke` (provider 429 storm) or `make chaos-matrix`
(all experiments, serialized). Network-fault experiments need the overlay —
export `COMPOSE_FILE="docker-compose.yml:docker-compose.chaos.yml"` for the
whole run so the driver's orchestrator recreates keep the toxiproxy routes,
then `docker compose up -d`.
Deterministic storage failpoints: `cargo nextest run -p moa-session
--features failpoints --test session_db`. Every experiment ends
with the invariant sweep from `moa_test_support::invariants`.

## Operational rules learned from live runs

- **Never swap orchestrator code under a Restate that holds journaled
  invocations.** Rebuilding the image mid-campaign poisons replay (RT0016
  journal mismatch) and the infinite retries starve fresh traffic. Wipe
  `moa-restate-data` and re-register between code changes, exactly like the
  e2e scripts' ephemeral Restate.
- **Mock capacity runs exclude sandbox tool execution.** Sessions created via
  the SessionStore path carry the system-default agent context, which allows
  zero tool calls; scripted tool invocations only exercise the policy-denial
  and loop-guardrail paths. `scripts/realistic.json` therefore uses text-only
  completions paced like tool round-trips.
- **The generator is wedge-proof by design** (staleness shedding, bounded
  pool waits, pool self-healing, overlapped setup). If a run ever exceeds
  `duration + ~2 min`, treat it as a bug in the harness, not the system.
- Host ports 9000/9001 frequently collide (minio, k3d node ports). The
  `loadtest-mock` target defaults RustFS to 10090/10091; export
  `MOA_RUSTFS_PORT`/`MOA_RUSTFS_CONSOLE_PORT` to choose a different pair.

## Certification checklist (per release)

1. T1 gates green on the release candidate.
2. T2 knee within 10% of the recorded baseline; step-latency p95s attributed.
3. Chaos matrix green (all experiments recover, zero invariant violations).
4. Failpoint `_db` lane green.
5. For a 10k+ QPS claim: T3 merged report at the target rate with corrected
   p99 in budget and invariant sweep clean.

## Metrics wiring

- Local compose runs the development scrape exporter
  (`MOA_METRICS_EXPORTER=prometheus`, `MOA_METRICS_PROMETHEUS_LISTEN=0.0.0.0:9090`,
  hosts `10023` and `10001`), which is what makes a load run readable with
  `curl`. Kubernetes runs `MOA_METRICS_EXPORTER=otlp` and exposes no metrics port
  at all: a scrape through a non-sticky Service lands on an arbitrary replica
  each interval and blends unrelated processes into one series. Load numbers
  taken from a scraped k8s Service were never trustworthy, which is why that
  surface is gone rather than merely discouraged.
- Turn-step attribution: `moa_turn_step_duration_seconds{step=...}` with
  sub-10ms buckets (see `moa-observability/src/runtime_metrics.rs`).
- Event append phase attribution:
  `moa_session_event_append_phase_seconds{phase=...}` splits the durable
  append path into bounded phases for load reports, including distinct
  `acquire_connection` and `begin_transaction` waits.
- Tokio runtime gauges require a `tokio_unstable` build
  (`RUSTFLAGS="--cfg tokio_unstable"`); perf images should enable it.
- Baselines live in `docs/18-performance.md` and are updated from T2 runs.

# Fix Simplification Deferred Regressions

## Goal

Turn the deferred validation blockers in `docs/simplification-deferred-regressions.md`
into reliable local and CI gates without weakening the strict release checks.

## Current Diagnosis

The deferred items are four failure families, not nine unrelated bugs.

| Family | Items | What is happening |
|---|---:|---|
| Retrieval perf profile split | REG-001 | The `retrieval` perf gate is intentionally strict and exits on non-x86 AVX2 hosts before it can provide any local signal. The missing piece is a documented smoke profile for developer hardware, not a weaker release gate. While touching this path, also fix CI env naming so the workflow exports the `MOA_COHERE_API_KEY` variable read by the code. |
| Mock loadtest setup and signal quality | REG-002, REG-003, REG-004, REG-005 | `make loadtest-mock` assumes OpenFGA IDs have already been bootstrapped and sourced, can collide on common RustFS ports, and runs the raw loadtest binary rather than the gated `mock-short` perf profile. The raw path can report overload and still exit successfully. The low-scale `mock-short` gate also exercises nondeterministic tenancy/session setup; additionally `perf_gate --profile mock-short` currently accepts `--qps` but does not pass it into `MockSmokeConfig.rate`. |
| Isolated Postgres admin connection pressure | REG-006, REG-008 | DB-backed tests fail before product logic because isolated-store setup opens short-lived maintenance `PgPool`s with eager connections while several tests create/drop databases concurrently. Some observed runs may also have been pure environment failures when compose Postgres was not running, and raw `cargo test` bypasses nextest's DB throttling. The failure signature is setup-level pool timeout against the maintenance database. |
| Fixture-service E2E setup stability | REG-007, REG-009 | The live gate sometimes fails after core suites pass because fixture tests do expensive orchestrator binary build/setup inside the nextest per-test timeout. A separate intermittent container-port discovery error needs labeled retry/diagnostic handling, because both Restate and OpenFGA ask testcontainers for an `8080/tcp` mapping and the current error does not identify which container lost its mapping. |

## Non-Goals

- Do not relax the strict retrieval release profile.
- Do not remove the load/perf/live gates to get a green run.
- Do not add compatibility shims for old local setup behavior.
- Do not broaden this work into unrelated simplification-audit implementation.

## Execution Plan

### 1. Stabilize Fixture-Service E2E First

This restores the most important periodic gate before touching load/perf code.

Files likely to change:

- `scripts/run-clean-e2e.sh`
- `crates/moa-test-support/src/orchestrator_fixture.rs`
- `crates/moa-test-support/src/orchestrator_fixture/process.rs`
- `.config/nextest.toml`

Steps:

1. Move the shared `cargo build -p moa-orchestrator --bin moa-orchestrator-bin`
   step before the `fixture-service-e2e` profile in `scripts/run-clean-e2e.sh`.
2. Export `MOA_ORCHESTRATOR_BIN` to the built binary before invoking
   `run_without_external_orchestrator cargo nextest run ... --profile fixture-service-e2e`.
3. Align fixture binary features with the live script's orchestrator feature set
   or keep one explicit fixture feature list if runtime behavior requires it.
4. Add a fixture-profile timeout override only if prebuilding still leaves
   legitimate test execution near the current 240 second limit.
5. Improve the testcontainer port-discovery error path with a small helper
   around `get_host_port_ipv4` that retries briefly and labels the container
   mapping, such as `restate ingress` or `openfga api`.
6. Include container ID, requested port, and available diagnostic context in
   the error so the next REG-007 failure is actionable.

Acceptance commands:

```bash
MOA_RUN_LIVE_E2E=1 scripts/run-clean-e2e.sh --live
env -u MOA_RESTATE_INGRESS_URL -u MOA_RESTATE_ADMIN_URL -u MOA_RESTATE_DEPLOYMENT_URI -u MOA_DATABASE_URL \
  cargo nextest run -p moa-orchestrator --locked \
  --features provider-overrides,skill-learning,integration \
  --profile fixture-service-e2e --run-ignored ignored-only
```

For a cold-target regression check:

```bash
TMP_TARGET="$(mktemp -d /tmp/moa-fixture-target.XXXXXX)"
CARGO_TARGET_DIR="$TMP_TARGET" cargo build -p moa-orchestrator \
  --bin moa-orchestrator-bin --features provider-overrides,skill-learning --locked
env -u MOA_RESTATE_INGRESS_URL -u MOA_RESTATE_ADMIN_URL -u MOA_RESTATE_DEPLOYMENT_URI -u MOA_DATABASE_URL \
  CARGO_TARGET_DIR="$TMP_TARGET" \
  MOA_ORCHESTRATOR_BIN="$TMP_TARGET/debug/moa-orchestrator-bin" \
  cargo nextest run -p moa-orchestrator --locked \
  --features provider-overrides,skill-learning,integration \
  --profile fixture-service-e2e --run-ignored ignored-only \
  -E 'test(action_policy_flow_covers_auto_review_decision_and_member_authz)'
```

Expected result:

- REG-009 closes when fixture tests no longer cold-build inside the test timeout.
- REG-007 can be downgraded to "watch" only after the port diagnostics are in
  place and one full live gate passes.

### 2. Fix Isolated Postgres Maintenance Connections

This unblocks DB-backed verification lanes for memory PII, edge analytics,
session analytics, and orchestrator session-store behavior.

Files likely to change:

- `crates/moa-session/src/testing.rs`
- `crates/moa-test-support/src/postgres.rs`
- affected DB test harnesses only if they leak isolated stores

Steps:

1. Establish the operational baseline first: start compose Postgres, confirm the
   `127.0.0.1:10040` port is reachable, and rerun the failing lanes serialized
   with `--test-threads=1` or through the matching nextest DB profile.
1. Replace short-lived maintenance `PgPool` creation with a single admin
   connection or a non-eager pool with `min_connections(0)` and `max_connections(1)`.
2. Serialize template clone/drop admin operations with a narrow async semaphore
   if local Postgres still reports contention under concurrent tests.
3. Audit `drop_isolated_test_database` callers and test fixtures to ensure every
   isolated database is dropped and every store pool is closed before drop.
4. Add a focused concurrency regression test for repeated isolated store
   creation and cleanup against the compose Postgres service.
5. Keep the default test database URL behavior simple: one maintenance URL,
   cloned per-test databases, no extra compatibility constructor.

Acceptance commands:

```bash
docker compose up -d postgres
docker compose ps postgres
nc -z 127.0.0.1 10040
MOA_DATABASE_URL=postgres://moa_owner:dev@127.0.0.1:10040/moa \
  cargo test -p moa-memory-pii --locked erasure_db_memory -- --nocapture --test-threads=1
MOA_DATABASE_URL=postgres://moa_owner:dev@127.0.0.1:10040/moa \
  cargo test -p moa-edge --test direct_read_routes_db --locked -- --nocapture --test-threads=1
MOA_DATABASE_URL=postgres://moa_owner:dev@127.0.0.1:10040/moa \
  cargo test -p moa-session --test postgres_store_db --locked \
  postgres_materialized_analytics_views_refresh -- --ignored --exact --nocapture --test-threads=1
MOA_DATABASE_URL=postgres://moa_owner:dev@127.0.0.1:10040/moa \
  cargo test -p moa-orchestrator --lib --locked session_store -- --nocapture --test-threads=1
```

Expected result:

- REG-006 and REG-008 close when failures, if any, move past isolated-store
  setup and into the actual product assertions.

### 3. Make Mock Loadtest Bootstrap Deterministic

This makes the local load target fail early for setup problems and run with the
same authz prerequisites as normal development.

Files likely to change:

- `Makefile`
- `docker-compose.yml` or compose override docs if RustFS ports are only defined there
- `crates/moa-loadtest/README.md`
- possibly a small helper under `scripts/`

Steps:

1. Make `make loadtest-mock` run or require `make fga-bootstrap` before
   restarting the orchestrator.
2. Source `.env.fga` inside the make target for the orchestrator/loadtest
   subprocesses instead of requiring the caller to remember it.
3. Add a preflight for RustFS host ports, or assign safer local loadtest default
   ports through `MOA_RUSTFS_PORT` and `MOA_RUSTFS_CONSOLE_PORT`.
4. Fail before Docker rebuilds when required local services, generated authz IDs,
   or port bindings are unavailable.
5. Change `make loadtest-mock` to run the gated
   `perf_gate --profile mock-short` path instead of the raw `moa-loadtest`
   binary.
6. Document the target as a local smoke, not a capacity benchmark.

Acceptance commands:

```bash
make loadtest-mock
MOA_RUSTFS_PORT=10090 MOA_RUSTFS_CONSOLE_PORT=10091 make loadtest-mock
```

Expected result:

- REG-002 closes when the target bootstraps or clearly fails before startup.
- REG-003 closes when common 9000/9001 port collisions no longer block the
  default local smoke path.

### 4. Split Mock Smoke From Capacity Load

This turns the mock target into a meaningful pass/fail regression signal.

Files likely to change:

- `crates/moa-loadtest/src/bin/perf_gate.rs`
- `crates/moa-loadtest/src/scenarios/mock_smoke.rs`
- `crates/moa-loadtest/src/backend.rs`
- `.github/workflows/perf-gate.yml`
- `.github/workflows/deploy.yml`
- `Makefile`

Steps:

1. Wire `perf_gate --profile mock-short --qps` into `MockSmokeConfig.rate`.
2. Lower the default local smoke rate and keep any high-rate capacity profile
   behind an explicit `loadtest-capacity` target or flag.
3. Make `make loadtest-mock` enforce the same p95/error thresholds as the
   `mock-short` perf gate, so an overloaded report cannot exit green.
4. Trace the `403 not participant` setup path by checking the loadtest identity,
   session creator subject, participant grant, and OpenFGA visibility before
   turns are appended.
5. Make session setup deterministic: grant the exact subject used for
   `create_session`, verify or await visibility, then start the measured turn
   window.
6. Remove or repair stale duplicate CI smoke jobs so CI uses one script/profile
   for the mock gate. In particular, make `.github/workflows/deploy.yml` either
   call the same stack setup as `.github/workflows/perf-gate.yml` or stop
   running a duplicate mock-smoke job without the needed services.

Acceptance commands:

```bash
cargo test -p moa-loadtest --locked
cargo run --release -p moa-loadtest --bin perf_gate -- \
  --profile mock-short --endpoint http://localhost:10010 \
  --duration 20s --vus 1 --qps 1 --max-p95-ms 5000 --max-error-rate 0.01 \
  --metrics-endpoint http://localhost:10023/metrics \
  --prom-out target/perf-gate/mock-short.prom
make loadtest-mock
```

Expected result:

- REG-004 closes when degraded local load reports fail the target.
- REG-005 closes when low-scale `mock-short` either passes deterministically or
  fails on product latency/errors rather than auth/session setup.

### 5. Add A Developer Retrieval Smoke Profile

This gives memory/RAG simplification work a bounded local signal while retaining
the strict CI release gate.

Files likely to change:

- `crates/moa-loadtest/src/bin/perf_gate.rs`
- `crates/moa-loadtest/src/scenarios/retrieval/config.rs`
- `crates/moa-loadtest/src/scenarios/retrieval/mod.rs`
- `.github/workflows/perf-gate.yml`
- `crates/moa-loadtest/README.md`
- `docs/18-performance.md`
- `docs/simplification-deferred-regressions.md`

Steps:

1. Add a distinct `retrieval-smoke` perf-gate profile or equivalent named mode.
2. Keep `retrieval` enforcing the current CPU/memory/AVX2 hardware floor.
3. Make the smoke profile use small tenant/fact/QPS defaults and skip the AVX2
   hardware floor, while still enforcing correctness, cache-hit, and broad
   latency budgets appropriate for developer machines.
4. Add tests around profile-to-config mapping so the strict profile cannot
   accidentally lose its hardware floor.
5. Document the intended use: local smoke after RAG/retrieval cuts, strict
   retrieval gate in CI or on capable hardware.
6. Correct the perf-gate workflow secret export from `COHERE_API_KEY` to
   `MOA_COHERE_API_KEY`, matching the runtime config path used by providers.

Acceptance commands:

```bash
cargo test -p moa-loadtest --locked retrieval
cargo run --release -p moa-loadtest --bin perf_gate -- \
  --profile retrieval-smoke --tenants 2 --facts-per-tenant 50 \
  --qps 5 --duration 15s --max-p95-ms 1000 \
  --p99-soft-target-ms 2000 --cache-hit-floor 0.50 \
  --prom-out target/perf-gate/retrieval-smoke.prom
```

Expected result:

- REG-001 closes when non-x86 developers have a documented local smoke and the
  strict retrieval release profile remains unchanged.

## Gate Cadence

Run focused tests after each phase. Run expensive gates at these points:

1. After fixture-service fixes: `MOA_RUN_LIVE_E2E=1 make e2e-clean-live`.
2. After DB maintenance fixes: affected DB lanes only, then live E2E if any
   shared session-store code changed materially.
3. After mock loadtest fixes: `make loadtest-mock` and `perf_gate --profile mock-short`.
4. After retrieval-smoke work: `retrieval-smoke` locally; strict `retrieval`
   only on a machine that satisfies the hardware floor or in CI.

## Closeout Checklist

- [x] REG-001 has a local retrieval smoke profile and strict retrieval remains strict.
- [x] REG-002 no longer depends on caller-sourced `.env.fga`.
- [x] REG-003 has safe local RustFS port handling.
- [x] REG-004 makes degraded mock load reports fail.
- [x] REG-005 has deterministic auth/session setup.
- [x] REG-006 DB-memory PII setup passes or reaches real assertions.
- [x] REG-007 has better fixture container diagnostics and a passing live gate.
- [x] REG-008 DB analytics/session-store setup passes or reaches real assertions.
- [x] REG-009 fixture E2E no longer compiles inside the per-test timeout.
- [x] `make chaos-smoke` inherits the safe RustFS port defaults and passes.
- [x] `docs/simplification-deferred-regressions.md`, `progress.md`, and
  `findings.md` are updated after each closed item.

## Implementation Results

- Fixture-service E2E now prebuilds `moa-orchestrator-bin` before the fixture
  profile, exports `MOA_ORCHESTRATOR_BIN`, and reuses that binary for the later
  shared-orchestrator smoke. Port mapping lookups now have labeled retries and
  richer diagnostics.
- Isolated Postgres maintenance work now uses a single admin connection instead
  of short-lived eager pools, and setup errors include clearer local compose
  hints.
- `make loadtest-mock` now bootstraps/sources OpenFGA, uses safer RustFS local
  ports, starts the required stack, and runs the gated `mock-short` perf profile.
- `chaos-smoke` and `chaos-matrix` now inherit the same safer RustFS local
  ports when their tests recreate compose services.
- `perf_gate` has a distinct `retrieval-smoke` profile that skips only the local
  hardware floor; strict `retrieval` keeps its release defaults and hardware
  requirements.
- `mock-short --qps` now maps to the mock smoke request rate.
- The perf workflow now exports `MOA_COHERE_API_KEY`, matching the runtime
  config path, and CI has one mock-smoke path instead of a stale duplicate.
- Validation found and fixed a false-positive retrieval-smoke vector oracle:
  scoped tenant-B KNN hits are valid, so the oracle now fails only on returned
  UIDs that are not visible in tenant B's scoped graph/vector rows.

## Final Verification

```bash
cargo test -p moa-loadtest --locked
cargo clippy -p moa-loadtest --all-targets --locked -- -D warnings
MOA_DATABASE_URL=postgres://moa_owner:dev@127.0.0.1:10040/moa \
  cargo test -p moa-memory-vector --test memory_vector_db_memory --locked \
  cross_tenant_knn_cannot_see_other_workspace_vectors -- --nocapture --test-threads=1
MOA_DATABASE_URL=postgres://moa_owner:dev@127.0.0.1:10040/moa \
  cargo run --release -p moa-loadtest --bin perf_gate -- \
  --profile retrieval-smoke --tenants 2 --facts-per-tenant 50 \
  --qps 5 --duration 15s --max-p95-ms 1000 \
  --p99-soft-target-ms 2000 --cache-hit-floor 0.50 \
  --prom-out target/perf-gate/retrieval-smoke.prom
make loadtest-mock
set -a; . ./.env.fga; set +a; make chaos-smoke
MOA_RUN_LIVE_E2E=1 make e2e-clean-live
git diff --check
```

Observed outcomes:

- `retrieval-smoke`: all gates green, 75/75 successful requests, P95 9.1ms,
  cache hit 1.000, 665 RLS attack attempts, 0 leaks.
- `make loadtest-mock`: all mock-short gates green, 16 completed sessions, 0
  failures, corrected P95 632.3ms, turn error rate 0.0000.
- `make chaos-smoke`: provider 429 storm passed, 1/1 test passing in 92.845s.
- `MOA_RUN_LIVE_E2E=1 make e2e-clean-live`: passed end-to-end in 108:16,
  including 13/13 Restate service E2E, 5/5 fixture-service E2E, and 13/13
  orchestrator-service E2E.

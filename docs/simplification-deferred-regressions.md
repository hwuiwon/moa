# Simplification Deferred Regressions

This file tracks regressions, degradations, and validation blockers found while
simplifying the codebase. The items below were repaired on 2026-07-04 as part of
the deferred-regression fix plan.

## Open Items

No open deferred regressions are currently tracked.

## Resolved Items

| ID | Surface | Status | Original Evidence | Resolution Evidence |
|---|---|---|---|---|
| REG-001 | Retrieval perf gate hardware floor | Resolved | The strict `retrieval` profile built, then exited before running on this machine with `hardware floor unmet: x86_64 with AVX2 is required`. | Added `retrieval-smoke`, kept strict `retrieval` hardware-floor enforcement, and documented both paths. `retrieval-smoke` passed live against Postgres/pgvector/Cohere: 75/75 requests, P95 9.1ms, cache hit 1.000, 665 RLS attack attempts, 0 leaks. |
| REG-002 | `make loadtest-mock` OpenFGA env handling | Resolved | `make loadtest-mock` timed out in readiness because the orchestrator started without generated `MOA_AUTHZ_OPENFGA_STORE_ID`/model env. | `make loadtest-mock` now bootstraps OpenFGA, sources `.env.fga`, starts the needed local services, and runs the gated `mock-short` profile. The target passed end-to-end. |
| REG-003 | RustFS host port collision during loadtest | Resolved | The target initially failed on host ports `9000`/`9001` already being allocated. | The local mock loadtest and chaos paths now default RustFS to safer ports `10090`/`10091`; `make loadtest-mock` and `make chaos-smoke` passed with those defaults. |
| REG-004 | Fixed-rate mock loadtest overloads local stack | Resolved | The raw mock loadtest exited successfully despite 14.87% turn error rate, 446 dropped turns, and corrected P95 33.01s. | `make loadtest-mock` now runs `perf_gate --profile mock-short` with thresholds. The repaired target passed with 16 completed sessions, 0 failed, turn error rate 0.0000, corrected P95 632.3ms. |
| REG-005 | Low-scale mock perf gate session setup/auth failures | Resolved | Low-scale `mock-short` failed from session setup/authz (`403 forbidden ... not participant`) rather than product latency. | The mock target now performs deterministic local bootstrap before the measured gate. `make loadtest-mock` passed the low-rate gated path with no session failures. |
| REG-006 | PII DB-memory erasure test database pool | Resolved | PII DB-memory erasure failed during isolated Postgres store creation with `pool timed out while waiting for an open connection`. | Isolated test maintenance work now uses a single admin connection with clearer compose hints. `hard_purge_contact_candidates_writes_summary_under_app_role_db_memory` passed against local compose Postgres. |
| REG-007 | Live E2E fixture-service container port discovery | Resolved | A live gate intermittently failed in fixture-service E2E with `container ... does not expose port 8080/tcp`. | Fixture port discovery now uses labeled retries and richer diagnostics. The full clean live gate passed, including fixture-service E2E 5/5. |
| REG-008 | Analytics/session-store DB verification pool | Resolved | Edge analytics, session analytics, and orchestrator session-store DB tests failed during isolated-store setup with maintenance pool timeouts. | Isolated maintenance setup was repaired and the affected DB lanes passed: edge direct-read routes 4/4, session materialized analytics refresh, orchestrator session-store library DB cases 11/11, plus the full clean live gate DB lanes. |
| REG-009 | Fixture-service E2E compile-budget timeout | Resolved | Fixture-service E2E timed out at 240s because the action-policy test spent its timeout budget cold-compiling the spawned orchestrator dependency graph. | `run-clean-e2e.sh` prebuilds `moa-orchestrator-bin`, exports `MOA_ORCHESTRATOR_BIN`, and reuses the binary for fixture/shared orchestrator lanes. Fixture-service E2E passed 5/5 in the full clean live gate. |
| REG-010 | Clean E2E timing and local runner contention | Resolved | The Group D clean-E2E rerun was interrupted during setup because the Rust `moa-fga-bootstrap` helper and several Rust test/build-script binaries stalled at process startup while macOS `syspolicyd` was active and rust-analyzer held a workspace `cargo check --workspace --all-targets` artifact lock. A follow-up run showed the broader runner regression: `cargo test -p moa-orchestrator --tests` launched ignored or feature-disabled `*_service_e2e` binaries such as `long_conversation_cost_service_e2e` and `procedure_memory_nodes_service_e2e`; sampling showed they were stalled at `_dyld_start` while running zero/ignored tests. The old timing report also mislabeled interrupted runs as `passed`. | `scripts/run-clean-e2e.sh` now records interrupted phases, bootstraps OpenFGA through `curl`/`jq` using `schema_v1.json`, replaces the broad cargo orchestrator preflight with nextest `fast-pr`, `db-session`, and `db-memory` phases, and runs feature-gated skill-learning binaries with the configured test thread count. A fresh patched `MOA_CLEAN_E2E_TEST_THREADS=4 ./scripts/run-clean-e2e.sh` passed in 06:26; the old broad cargo phase took 22:49 in the comparison run, while the replacement fast-pr/db-session/db-memory lanes took 00:02/02:28/01:01. |

## Validation Summary

- Repair plan: `docs/engineering-discipline/plans/2026-07-04-fix-simplification-deferred-regressions.md`.
- `cargo test -p moa-loadtest --locked` passed before the RLS-oracle adjustment.
- `cargo clippy -p moa-loadtest --all-targets --locked -- -D warnings` passed.
- `cargo test -p moa-loadtest --locked unique_match_uids_collapses_duplicate_vector_hits` passed.
- `cargo test -p moa-memory-vector --test memory_vector_db_memory --locked cross_tenant_knn_cannot_see_other_workspace_vectors -- --nocapture --test-threads=1` passed.
- `retrieval-smoke` passed live with 75/75 requests, P95 9.1ms, P99 12.3ms, cache hit 1.000, 665 RLS attack attempts, and 0 leaks.
- `make loadtest-mock` passed the gated mock-short profile with 16 completed sessions, 0 failures, corrected P95 632.3ms, and turn error rate 0.0000.
- `make chaos-smoke` passed the provider 429 storm lane with 1/1 test passing after the chaos target inherited the safe RustFS port defaults.
- `MOA_RUN_LIVE_E2E=1 make e2e-clean-live` passed end-to-end in 108:16.
- `MOA_CLEAN_E2E_TEST_THREADS=4 ./scripts/run-clean-e2e.sh` passed after the clean-E2E runner optimization in 06:26.

## Notes

- The strict `retrieval` profile still enforces the release hardware floor. Use
  `retrieval-smoke` for local developer validation on machines that cannot run
  the strict gate.
- During repair validation, the initial `retrieval-smoke` run exposed a false
  positive in the loadtest vector oracle: tenant B legitimately returned tenant
  B `Fact` vectors for tenant A's query vector, but the oracle treated any hit
  as a leak. The oracle now verifies returned UIDs are visible in the scoped
  tenant graph/vector rows; tenant-B hits are accepted, off-scope hits still fail.

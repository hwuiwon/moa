# Cloud API Boundary Completion Review

> This document replaces the original command-client boundary plan. The
> original plan kept a thin local command client. The accepted product direction
> is stricter: remove the local command client and its wrapper/runtime crates
> entirely, while preserving every feature through hosted API requests.

## Current Goal

MOA is cloud-first. Product behavior runs in `moa-orchestrator`, `moa-edge`,
gateway adapters, or server-side worker crates. Local and automated tests call
hosted APIs directly through HTTP/Restate ingress helpers.

No local command client owns session orchestration, memory retrieval, lineage,
privacy export/erase, eval execution, vector promotion, checkpoint management,
approvals, authz, tool execution, sandbox setup, filesystem setup, or database
access.

## Completion Matrix

| Original Task | Current Status | Evidence |
|---|---|---|
| 1. Remove daemon/local runtime naming | Done by stronger removal | The command client and runtime wrapper crates are not workspace members. Current docs describe `moa-orchestrator` and hosted APIs as the runtime boundary. |
| 2. Add shared wire DTOs | Done | Hosted analytics, memory, lineage, privacy, skills, eval, and admin DTOs live in `crates/moa-core/src/wire.rs`. |
| 3. Analytics cloud service | Done | `crates/moa-orchestrator/src/services/analytics.rs`; public edge routes under `/v1/analytics/*`; tests in `crates/moa-orchestrator/tests/analytics.rs` and `crates/moa-edge/src/routes.rs`. |
| 4. Memory cloud service | Done | `crates/moa-orchestrator/src/services/memory.rs`; public edge routes under `/v1/memory/*`; tests in `crates/moa-orchestrator/tests/memory_service.rs` and `crates/moa-edge/src/routes.rs`. |
| 5. Lineage and privacy cloud services | Done | `crates/moa-orchestrator/src/services/lineage_admin.rs` and `crates/moa-orchestrator/src/services/privacy.rs`; public edge routes under `/v1/lineage/*` and `/v1/privacy/*`; tests in `lineage_admin.rs`, `privacy_service.rs`, and edge route tests. |
| 6. Skills cloud service | Done | `crates/moa-orchestrator/src/services/skills.rs`; public edge routes under `/v1/skills/*`; tests in `crates/moa-orchestrator/tests/skills_service.rs` and edge route tests. |
| 7. Eval cloud service and workflow | Done | `crates/moa-orchestrator/src/services/eval.rs` and `crates/moa-orchestrator/src/workflows/eval_run.rs`; public edge routes under `/v1/evals/*`; tests in `crates/moa-orchestrator/tests/eval_service.rs` and edge route tests. |
| 8. Admin maintenance cloud APIs | Done | `crates/moa-orchestrator/src/services/admin_maintenance.rs`; public edge routes under `/v1/admin-maintenance/*`; tests in `crates/moa-orchestrator/tests/admin_maintenance.rs` and edge route tests. |
| 9. Remove direct server-side command-client dependencies | Done by stronger removal | The command-client, runtime wrapper, and HTTP wrapper crates are gone from `Cargo.toml`, `Cargo.lock`, and `cargo metadata`. Test/load helpers use direct HTTP APIs. |
| 10. Update documentation and markdown | Done | Root docs, operational runbooks, agent guidance, and this plan describe hosted APIs. Current `rg` checks exclude only third-party tool names such as the OpenFGA tool. |
| 11. End-to-end verification | Done | Deterministic tests, workspace build/lint, and selected Restate e2e checks passed during implementation. |

## Preserved Feature Surface

| Feature Group | Hosted API Surface |
|---|---|
| Session lifecycle and turns | `SessionStore`, `Session`, and `TurnExecution` Restate APIs; test helpers call these through HTTP. |
| Tool execution and sandbox/code execution | `ToolExecutor`, `Hands`, and orchestrator turn execution. |
| Approvals | `Approvals` service and `/v1/approvals` edge routes. |
| API keys and authz admin | `ApiKeys` and `Authz` services; `/v1/authz/tuple-write` edge route for tuple writes. |
| Agent templates and agents | `AgentTemplates` and `Agents` services with public edge routes. |
| Tenant audit settings and verification | `Tenants` and `Audit` Restate services. |
| Analytics | `Analytics` service and `/v1/analytics/*` edge routes. |
| Memory | `Memory` service and `/v1/memory/*` edge routes. |
| Lineage and DSAR/privacy | `LineageAdmin`, `Privacy`, `/v1/lineage/*`, and `/v1/privacy/*`. |
| Skills | `Skills` service and `/v1/skills/*`. |
| Eval | `Eval` service, `EvalRun` workflow, and `/v1/evals/*`. |
| Vector promotion and checkpoints | `AdminMaintenance` service and `/v1/admin-maintenance/*`. |

## Verification Commands

Use these commands for the current no-command-client boundary:

```bash
cargo fmt --all
cargo test -p moa-orchestrator --tests -- --test-threads=1
cargo test -p moa-edge routes::tests -- --test-threads=1
cargo test -p moa-test-support --tests
cargo test -p moa-loadtest --tests
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo build --workspace
git diff --check
```

For local Restate e2e, use a clean temporary Postgres database if the long-lived
development database has migration checksum drift:

```bash
set -a
. ./.env.fga
set +a
MOA_RESTATE_DEPLOYMENT_HOST=host.docker.internal \
RESTATE_ADMIN_URL=http://127.0.0.1:10011 \
RESTATE_INGRESS_URL=http://127.0.0.1:10010 \
TEST_DATABASE_URL=postgres://moa_owner:dev@127.0.0.1:10040/<temporary_database> \
cargo test -p moa-orchestrator --test integration session_store_e2e::session_store_round_trip_through_restate -- --ignored --test-threads=1
```

## Audit Greps

```bash
rg -n -e '<removed command-client crate names>' -e '<removed command-client commands>' .
cargo metadata --no-deps --format-version=1 | jq -r '.packages[].name'
```

Expected: no removed MOA command-client or wrapper package references remain.

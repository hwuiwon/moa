# moa-orchestrator

Restate-backed orchestrator for MOA: the `moa-orchestrator-bin` binary hosts
the platform's Restate services, workflows, and virtual objects, and bridges
Restate handlers to the brain's turn engine. It also spawns non-Restate
background work such as the ClickHouse analytics exporter
(`moa-analytics-export`).

Database migration is a separate process phase:

```bash
MOA_DATABASE_URL=postgres://runtime-role@... \
MOA_DATABASE_ADMIN_URL=postgres://migration-role@... \
  cargo run -p moa-orchestrator --bin moa-orchestrator-bin -- migrate
MOA_DATABASE_URL=postgres://runtime-role@... \
  cargo run -p moa-orchestrator --bin moa-orchestrator-bin
```

The default runtime command validates the exact complete central migration
history through `MOA_DATABASE_URL` before constructing runtime dependencies. It
never reads `MOA_DATABASE_ADMIN_URL` or applies migration DDL.

## Structure

- `services/` — Restate service modules (agents, skills, memory, execution,
  experiments, privacy, SCIM, tool executor, session store, and more).
- `workflows/` — Restate workflow modules (execution runs and tasks,
  experiment runs and trials, memory consolidation).
- `objects/` — Restate virtual objects (session, worker, tenant, cron jobs).
- `runtime/` — runtime composition for the binary: database, dependency
  wiring, Restate endpoint, background jobs, KMS, and channel ingress.
- Shared support: `ctx` (`OrchestratorCtx`), `config`, `turn`/`turn_driver`
  (workflow-native turn execution), `brain_bridge`, `guardrails` (LLM judge
  runner), `handlers` (authorization shims), `vo` (virtual-object state
  plumbing), and `lineage` (sink selection).

## Features

- `integration` — enables the shared orchestrator test fixture in
  `moa-test-support`.
- `provider-overrides` — enables the scripted LLM provider for deterministic
  tests.
- `execution-planning-failpoints` — deterministic execution-planning fault
  hooks used only by service E2E fixtures.
- `auth0` — Auth0 backend for `moa-auth-providers`.
- `slack` / `postmark` / `twilio` — optional messaging adapters in
  `moa-messaging`.

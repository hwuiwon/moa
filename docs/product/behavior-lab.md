# Behavior Lab

_Product boundary for behavior-lab artifacts, experiments, analytics, and live simulation._

Behavior Lab is the product surface for testing how target agents or execution
runs behave under simulated users, profiles, data bundles, and scenarios.
Behavior Lab is the only tenant evaluation product. It is not the
regression-eval system: regression evals remain in `moa-eval`, which is a
platform-only library, CLI, and `xtask` surface with no hosted service, no
tenant MCP tool, and no public route.

## Product Boundary

Behavior Lab uses existing typed services:

- `Artifacts` imports, validates, publishes, and exports experiment plan artifacts.
- `Experiments` generates plans, admits runs, lists runs and trials, cancels
  runs, proposes reviewed improvements, and reads score summaries.
- `ExperimentRun` owns run-level workflow orchestration.
- `ExperimentTrialRun` owns per-trial simulator turns and target execution.
- Direct edge analytics routes read product insights from scoped analytics views.

The public edge routes are:

| Route | Handler |
|---|---|
| `POST /v1/experiments/generate-plan` | `Experiments/generate_plan` |
| `POST /v1/experiments/run-plan` | `Experiments/run` |
| `POST /v1/experiments/status` | `Experiments/status` |
| `POST /v1/experiments/list` | `Experiments/list` |
| `POST /v1/experiments/plans/list` | `Experiments/list_plans` |
| `POST /v1/experiments/trials` | `Experiments/trials` |
| `POST /v1/experiments/trial-status` | `Experiments/trial_status` |
| `POST /v1/experiments/cancel` | `Experiments/cancel` |
| `POST /v1/experiments/propose-improvements` | `Experiments/propose_improvements` |
| `POST /v1/experiments/scores` | `Experiments/scores` |
| `POST /v1/experiments/compare` | `Experiments/compare` |
| `POST /v1/experiments/agent-revision-simulations` | `Experiments/run_agent_revision_simulation` |
| `POST /v1/experiments/agent-revision-simulations/compare` | `Experiments/compare_agent_revision_simulation` |
| `POST /v1/agent-simulations` | `Experiments/run_agent_revision_simulation` |
| `POST /v1/agent-simulations/{run_uid}/compare` | `Experiments/compare_agent_revision_simulation` |

There is no public `/v1/evals/*` product route and no public
`/v1/experiments/run` alias.

Experiment execution is gated by the tenant operator/admin relation. Workspace
admins are represented in OpenFGA as `workspace#admin`, inherited into each
linked tenant's `tenant#admin`; the tenant `operator` relation includes tenant
admins.
Behavior Lab dashboards use `POST /v1/artifacts/list` and
`POST /v1/artifacts/export` for experiment plan artifact inspection, and direct
analytics reads through `GET /v1/analytics/catalog` and
`POST /v1/analytics/query`; analytics calls may include an explicit `tenant_id`
when the caller is authorized for that target tenant.

## Artifact Model

Behavior Lab uses the same `ArtifactDocument` envelope as skills, connectors,
and agents, but exposes one artifact kind:

| Kind | Purpose |
|---|---|
| `experiment_plan` | Trial matrix, variants, simulator model, budgets, scorecard, and learning proposal settings |

Simulation inputs are typed embedded blocks under
`definition.spec.simulation`:

| Block | Purpose |
|---|---|
| `personas[]` | Simulated user voice, goals, constraints, and stop behavior |
| `profiles[]` | Structured account, user, order, or environment facts |
| `data_bundles[]` | Fixture, mock, connector-owned, or approved live data sources |
| `scenarios[]` | Starting situation, allowed intents, success/failure criteria, and scoring rubric |

Each block has a stable `id`. Trial rows, score breakdowns, analytics, and UI
state refer to these IDs, while `artifact_revision_uids` pin the exact
`experiment_plan` bytes used for the run.

Postgres stores the canonical artifact document as JSON. YAML is an
authoring/import/export format only; importing YAML produces the same typed
document and validation path as JSON.

## UI Round Trip

The top-level `ui` object and kind-specific `definition.spec.ui` objects are
non-semantic builder metadata. They may contain canvas layout, visual grouping,
labels, icons, and panel state. Execution, validation of behavior semantics,
score computation, and learning proposals must not depend on `ui`.

The UI must round-trip through `ArtifactDocument`: load the canonical JSON
document, preserve unknown `ui` fields, edit semantic fields through the typed
artifact structs, and export JSON or YAML from the same document model. A canvas
cannot own a separate execution model.

## Runs And Insights

An experiment plan expands into trials. `moa.experiment_run` stores the
run-level ledger, links pinned artifact revisions, and points at the run
`analytics.score_run`. `moa.experiment_trial` stores trial keys, variant,
the pinned plan revision, selected persona/profile/scenario/data-bundle IDs,
simulator settings, target session or execution-run links, trial score run ID,
stop reason, and trace ID.

Targets are `agent_loop` or `execution_run`. Agent-loop targets enter normal
`respond`/`act`/`run` routing through `TurnExecution`. Execution-run targets pin
a published skill's exact `execution_plan` template revision and use the same
`ExecutionRun`/`ExecutionTask` runtime as user work. Live trials never publish
generated plans or mutate skill revisions.

Score rows land in `analytics.scores`. `Experiments/scores` returns run score
summaries plus trial rollups and scenario breakdowns. `Experiments/compare`
returns run comparisons plus scenario and variant deltas. Direct edge analytics
routes own broader product insights; clients should not query raw SQL from the
UI.

Experiment-derived improvements cross one explicit review boundary:
`Experiments/propose_improvements` creates proposed `learning_candidates` and
may attach draft artifact revision IDs when it has a concrete reviewed patch to
preserve. Behavior Lab does not auto-promote skills, artifacts, or
learning entries. Promotion must happen through the relevant review surface, so
experiment evidence can inform a change without publishing or materializing it
as live behavior.

## Tenant-Operations MCP Adapter

The `/mcp` protected resource is a thin adapter over `Artifacts`,
`Experiments`, direct edge analytics reads, `Skills`, and other typed services.
It does not own Behavior Lab domain logic, bypass service authorization, or
publish `/v1/evals/*` semantics. It advertises no eval tools at all; the
regression harness is unreachable from `/mcp`. Persistent Behavior Lab plans
remain generic `experiment_plan` artifacts.

## Verification Commands

Default deterministic checks:

```bash
cargo nextest run -p moa-artifacts --all-targets --locked
cargo nextest run -p moa-scoring --all-targets --locked
cargo nextest run -p moa-experiments --all-targets --locked
cargo nextest run -p moa-orchestrator --test experiment_service --test behavior_lab_simulation_e2e --locked
cargo nextest run -p moa-edge --lib --locked
```

Ignored local Restate E2Es require Postgres, OpenFGA, Restate, and
`--features integration,provider-overrides`. They start the orchestrator with
`MOA_PROVIDERS_OVERRIDE=scripted:<fixture>` and do not use billed providers by
default.

```bash
cargo nextest run -p moa-orchestrator \
  --test behavior_lab_simulation_e2e \
  --features integration,provider-overrides \
  --locked \
  --run-ignored ignored-only
```

There is no live-provider simulation lane: these e2e tests run scripted
provider fixtures only.

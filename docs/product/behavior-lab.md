# Behavior Lab

_Product boundary for behavior-lab artifacts, experiments, analytics, and live simulation._

Behavior Lab is the product surface for testing how target agents or workflows
behave under simulated users, profiles, data bundles, and scenarios. It is not
the regression-eval system. Regression evals remain in `moa-eval`; the `Eval`
service and `EvalRun` workflow are internal-only and compiled behind
`internal-eval-runner`.

## Product Boundary

Behavior Lab uses existing typed services:

- `Artifacts` imports, validates, publishes, and exports experiment plan artifacts.
- `Experiments` generates plans, admits runs, lists runs and trials, cancels
  runs, proposes reviewed improvements, and reads score summaries.
- `ExperimentRun` owns run-level workflow orchestration.
- `ExperimentTrialRun` owns per-trial simulator turns and target execution.
- `Analytics` reads product insights from scoped analytics views.

The public edge routes are:

| Route | Handler |
|---|---|
| `POST /v1/experiments/generate-plan` | `Experiments/generate_plan` |
| `POST /v1/experiments/run-plan` | `Experiments/run` |
| `POST /v1/experiments/status` | `Experiments/status` |
| `POST /v1/experiments/list` | `Experiments/list` |
| `POST /v1/experiments/trials` | `Experiments/trials` |
| `POST /v1/experiments/trial-status` | `Experiments/trial_status` |
| `POST /v1/experiments/cancel` | `Experiments/cancel` |
| `POST /v1/experiments/propose-improvements` | `Experiments/propose_improvements` |
| `POST /v1/experiments/scores` | `Experiments/scores` |
| `POST /v1/experiments/compare` | `Experiments/compare` |

There is no default public `/v1/evals/*` product route and no public
`/v1/experiments/run` alias.

## Artifact Model

Behavior Lab uses the same `ArtifactDocument` envelope as skills, connectors,
and workflows, but exposes one artifact kind:

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
simulator settings, target session or workflow run links, trial score run ID,
stop reason, and trace ID.

Score rows land in `analytics.scores`. `Experiments/scores` returns run score
summaries plus trial rollups and scenario breakdowns. `Experiments/compare`
returns run comparisons plus scenario and variant deltas. `Analytics` owns
broader product insights; clients should not query raw SQL from the UI.

Experiment-derived improvements cross one explicit review boundary:
`Experiments/propose_improvements` creates proposed `learning_candidates`.
Behavior Lab does not auto-promote skills, workflows, or learning entries.

## Future MCP Adapter

Product/default MCP should be a thin adapter over `Artifacts`, `Experiments`,
`Analytics`, `Workflows`, and other typed services. It must not own Behavior
Lab domain logic, bypass service authorization, or publish public
`/v1/evals/*` semantics. If internal eval is exposed at all, it remains
`internal-eval-runner` gated.

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

Live simulation provider checks require an explicit gate and credentials:

```bash
MOA_RUN_LIVE_SIMULATION_TESTS=1 cargo nextest run -p moa-orchestrator \
  --test behavior_lab_simulation_e2e \
  --features integration,provider-overrides \
  --locked \
  --run-ignored ignored-only \
  live_behavior_lab_simulation_gate_requires_flag_and_provider_credentials
```

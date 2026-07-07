# Live Behavior Experiments

_Boundary between regression evals, production-path experiments, and analytics._

## Choose The Surface

Use a regression eval when the question is "did this behavior regress under a
controlled scenario?" Regression evals live in `moa-eval`. They use datasets,
transcripts, scripted providers, replay, and budget gates so CI and nightly
jobs can compare runs deterministically. The `Eval` service is an
internal-only Restate surface; hosted run status is persisted in Postgres.
Public edge builds do not translate `/v1/evals/*` product routes.

Use a live behavior experiment when the question is "what happens when this
variant uses real MOA execution paths?" Live experiments live in
`moa-experiments` and the hosted `Experiments` service. They store run metadata
in `moa.experiment_run`, then admit the target through existing production
paths: `Session`/`TurnExecution` for agent loops and the procedure runtime for
skill procedure runs.

Do not use live experiments as a shortcut for regression coverage. If a live
experiment reveals a repeatable failure, turn the finding into a regression
eval or a `learning_candidates` proposal with enough evidence to reproduce it.
The product-facing Behavior Lab artifact model is documented in
[`docs/product/behavior-lab.md`](../product/behavior-lab.md).

## Target Kinds

`agent_loop` targets contain a prompt, model, optional session ID, and
attachments. A run without a session ID creates a normal API session for the
authorized user or delegated agent identity, initializes the session virtual
object, and queues the prompt through `Session/queue_message`. Tool routing,
skills, memory, approvals, event logging, and learning emission are the same
production path used by user sessions.

`procedure` targets reference a published skill's procedure through a
`procedure_ref`, plus input JSON, an optional session ID, and an optional
idempotency key. Starting a procedure run first validates the input JSON against
the skill's input schema; a run with missing required inputs is rejected with a
structured missing-inputs error instead of being created. The experiment then
starts the procedure through the procedure runtime and stores the linked
`moa.artifact_run.run_uid`.

## Artifact Revisions

Experiment variants can pin `artifact_revision_uids`. The run stores those
revision IDs on `moa.experiment_run` for fast API reads and also records
enforceable links in `moa.experiment_run_artifact_revision`. Pin revisions when
testing a skill variant so later score comparisons can identify the
exact artifact bytes under test.

## Scores

Each accepted experiment has a `score_run_id`. If the request omits it,
`Experiments/run` creates one and stores it in `analytics.score_run` with source
`experiment_run`. Score rows themselves land in `analytics.scores` under that
run ID.

Public experiment score APIs are experiment-run centric:

- `Experiments/scores` accepts `run_uid`, resolves the scoped
  `score_run_id`, then reads `analytics.scores`.
- `Experiments/compare` accepts `base_run_uid` and `new_run_uid`, resolves both
  score run IDs, then reuses the same tenant-scoped score comparison helper as
  internal eval and scoring surfaces.

## Public Product Routes

The public edge exposes product-shaped experiment routes and forwards them to
the `Experiments` Restate service:

| Public route | Service handler |
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

Agent-revision simulation routes are also backed by `Experiments`, but are
documented separately because they compare installed agent revisions rather than
generic experiment plans:

| Public route | Service handler |
|---|---|
| `POST /v1/experiments/agent-revision-simulations` | `Experiments/run_agent_revision_simulation` |
| `POST /v1/experiments/agent-revision-simulations/compare` | `Experiments/compare_agent_revision_simulation` |
| `POST /v1/agent-simulations` | `Experiments/run_agent_revision_simulation` |
| `POST /v1/agent-simulations/{run_uid}/compare` | `Experiments/compare_agent_revision_simulation` |

Do not document `/v1/experiments/run` as public; the public admission route is
`/v1/experiments/run-plan`.

## Action Policy

Live behavior experiments use the normal action-policy engine. Agent-loop
experiments do not enter a blocking review status; an admin-review decision
records a tenant action review and the target session continues. Procedure
experiment status mirrors the linked procedure run when a procedure run
has been attached.

## Learning Boundary

Experiment-derived improvements must go through `learning_candidates`; they
must never auto-promote skills. Experiment execution records run
metadata and links to sessions, procedure runs, artifact revisions, and score
runs. The explicit `Experiments/propose_improvements` operation may create a
candidate from completed experiment evidence, but review, evaluation, and
promotion remain separate explicit steps.

## Local Deterministic Commands

Use nextest for deterministic behavior-lab checks:

```bash
cargo nextest run -p moa-artifacts --all-targets --locked
cargo nextest run -p moa-scoring --all-targets --locked
cargo nextest run -p moa-experiments --all-targets --locked
cargo nextest run -p moa-orchestrator --test experiment_service --test behavior_lab_simulation_e2e --locked
cargo nextest run -p moa-edge --lib --locked
```

The `behavior_lab_simulation_e2e` target compiles in the default lane; its
local Restate E2E tests are ignored. To run those ignored tests, start the local
Postgres/OpenFGA/Restate dependencies first and compile the orchestrator with
provider overrides:

```bash
cargo nextest run -p moa-orchestrator \
  --test behavior_lab_simulation_e2e \
  --features integration,provider-overrides \
  --locked \
  --run-ignored ignored-only
```

The ignored local Restate E2Es need Postgres, OpenFGA, and Restate. They do not
use billed providers by default: the harness starts the orchestrator with
`MOA_PROVIDERS_OVERRIDE=scripted:<fixture>` and removes live provider keys from
the child process. Production builds do not compile provider overrides, and the
override is blocked in production environments.

## Live And Billed Gates

Live simulation provider tests are ignored by default and double-gated. Run
them only with an explicit opt-in and provider credentials:

```bash
MOA_RUN_LIVE_SIMULATION_TESTS=1 cargo nextest run -p moa-orchestrator \
  --test behavior_lab_simulation_e2e \
  --features integration,provider-overrides \
  --locked \
  --run-ignored ignored-only \
  live_behavior_lab_simulation_gate_requires_flag_and_provider_credentials
```

If `MOA_RUN_LIVE_SIMULATION_TESTS=1` is set but no `MOA_ANTHROPIC_API_KEY`,
`MOA_OPENAI_API_KEY`, or `MOA_GOOGLE_API_KEY` is present, the test fails before any
provider call. Do not put live or billed experiment checks in the default test
lane.

## Authorization

| Surface | Required authorization |
|---|---|
| Internal `Eval` suites, plan, run, status, datasets, replay, scores, compare | `Tenant:Operator`, which includes tenant admins and workspace admins |
| `Eval/execute_run` | Internal dispatch token created by `Eval/run` |
| `Experiments/generate_plan`, `run`, `cancel`, `propose_improvements` | `Tenant:Operator`, which includes tenant admins and workspace admins |
| `Experiments/status`, `list`, `list_plans`, `trials`, `trial_status`, `scores`, `compare` | `Tenant:Operator`, which includes tenant admins and workspace admins |
| `Experiments/run_agent_revision_simulation`, `compare_agent_revision_simulation` | `Tenant:Operator`, which includes tenant admins and workspace admins |
| direct edge `GET /v1/analytics/catalog` | `Tenant:Operator` |
| direct edge `POST /v1/analytics/query` | `Tenant:Operator` |
| direct edge `lineage/explain`, `query`, `verify` | tenant-authorized direct edge read |

Future MCP support is a thin adapter over product/default typed services such
as `Experiments`, direct edge analytics/lineage reads, and `Skills`. If it exposes
internal eval at all, that surface must remain explicitly internal. MCP must
forward through the same DTOs and authorization boundaries instead of owning
eval, experiment, analytics, lineage, or learning domain logic.

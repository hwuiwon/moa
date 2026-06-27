# Live Behavior Experiments

_Boundary between regression evals, production-path experiments, and analytics._

## Choose The Surface

Use a regression eval when the question is "did this behavior regress under a
controlled scenario?" Regression evals live in `moa-eval`. They use datasets,
transcripts, scripted providers, replay, and budget gates so CI and nightly
jobs can compare runs deterministically. The `Eval` service is an
internal-only surface compiled behind `internal-eval-runner`; hosted run status
is persisted in Postgres. Default public edge builds do not translate
`/v1/evals/*`.

Use a live behavior experiment when the question is "what happens when this
variant uses real MOA execution paths?" Live experiments live in
`moa-experiments` and the hosted `Experiments` service. They store run metadata
in `moa.experiment_run`, then admit the target through existing production
paths: `Session`/`TurnExecution` for agent loops and `WorkflowRuntime` for
artifact-backed workflow runs.

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

`workflow` targets contain a `workflow_ref`, input JSON, optional session ID,
and optional idempotency key. The experiment starts a published workflow through
`WorkflowRuntime` and stores the linked `moa.artifact_run.run_uid`. Workflow
experiments do not interpret workflow nodes themselves. Current workflow runs
may remain queued until the `moa-workflows` node interpreter supports execution.

## Artifact Revisions

Experiment variants can pin `artifact_revision_uids`. The run stores those
revision IDs on `moa.experiment_run` for fast API reads and also records
enforceable links in `moa.experiment_run_artifact_revision`. Pin revisions when
testing a skill or workflow variant so later score comparisons can identify the
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
| `POST /v1/experiments/trials` | `Experiments/trials` |
| `POST /v1/experiments/trial-status` | `Experiments/trial_status` |
| `POST /v1/experiments/cancel` | `Experiments/cancel` |
| `POST /v1/experiments/propose-improvements` | `Experiments/propose_improvements` |
| `POST /v1/experiments/scores` | `Experiments/scores` |
| `POST /v1/experiments/compare` | `Experiments/compare` |

Do not document `/v1/experiments/run` as public; the public admission route is
`/v1/experiments/run-plan`.

## Action Policy

Live behavior experiments use the normal action-policy engine. Agent-loop
experiments do not enter a blocking review status; an admin-review decision
records a tenant action review and the target session continues. Workflow
experiment status mirrors the linked artifact workflow run when a workflow run
has been attached.

## Learning Boundary

Experiment-derived improvements must go through `learning_candidates`; they
must never auto-promote skills or workflows. Experiment execution records run
metadata and links to sessions, workflow runs, artifact revisions, and score
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

If `MOA_RUN_LIVE_SIMULATION_TESTS=1` is set but no `ANTHROPIC_API_KEY`,
`OPENAI_API_KEY`, or `GOOGLE_API_KEY` is present, the test fails before any
provider call. Do not put live or billed experiment checks in the default test
lane.

## Authorization

| Surface | Required authorization |
|---|---|
| Internal-gated `Eval` suites, plan, run, status, datasets list, replay, scores, compare | `Tenant:Member` |
| Internal-gated `Eval` dataset registration | `Tenant:Editor` |
| `Experiments/generate_plan`, `run`, `cancel`, `propose_improvements` | `Tenant:Editor` |
| `Experiments/status`, `list`, `trials`, `trial_status`, `scores`, `compare` | `Tenant:Member` |
| direct edge `analytics/session-stats` | `Session:Participant` |
| direct edge `analytics/tenant-stats`, `cache-stats`, `experiment-stats`, `session-search` | `Tenant:Member` |
| direct edge `analytics/tool-stats` | `Tenant:Member` |
| direct edge `analytics/learning-candidates` | `Tenant:Editor` |
| direct edge `lineage/explain`, `query`, `verify` | `Tenant:Member` |

Future MCP support is a thin adapter over product/default typed services such
as `Experiments`, direct edge analytics/lineage reads, and `Workflows`. If it exposes
internal eval at all, that surface must remain qualified as
`internal-eval-runner` gated. MCP must forward through the same DTOs and
authorization boundaries instead of owning eval, experiment, analytics,
lineage, learning, or workflow domain logic.

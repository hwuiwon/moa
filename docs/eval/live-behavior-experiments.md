# Live Behavior Experiments

_Boundary between regression evals, production-path experiments, and analytics._

## Choose The Surface

Use a regression eval when the question is "did this behavior regress under a
controlled scenario?" Regression evals live in `moa-eval`. They use datasets,
transcripts, scripted providers, replay, and budget gates so CI and nightly
jobs can compare runs deterministically. There is no hosted `Eval` Restate
service, tenant eval persistence, or `/v1/evals/*` product route.

Use a live behavior experiment when the question is "what happens when this
variant uses real MOA execution paths?" Live experiments live in
`moa-experiments` and the hosted `Experiments` service. They store run metadata
in `moa.experiment_run`, then admit the target through existing production
paths: `Session`/`TurnExecution` for dynamically routed agent loops and
`ExecutionRun` for durable typed plans.

Every product run names one published `experiment_plan` revision. Admission
projects the target, variant, scorecard, and resource envelope from that
immutable revision; raw single-target run payloads are not accepted.

Do not use live experiments as a shortcut for regression coverage. If a live
experiment reveals a repeatable failure, turn the finding into a regression
eval or a `learning_candidates` proposal with enough evidence to reproduce it.
The product-facing Behavior Lab artifact model is documented in
[`docs/product/behavior-lab.md`](../product/behavior-lab.md).

## Target Kinds

`agent_loop` targets contain a prompt, model, optional agent selector, and
attachments. They carry no session ID: an experiment never continues a
caller-owned conversation, so every run creates its own eval-owned session for
the authorized user or delegated agent identity, initializes the session virtual
object, and submits the prompt through `Session/start_turn`. Tool routing,
skills, memory, approvals, event logging, and learning emission are the same
production path used by user sessions.

`execution_run` targets identify an activated skill's exact pinned
`execution_plan` template, plus input JSON, an optional session ID, and an
idempotency key. Raw plan JSON and compiled plan IDs are not accepted. Starting
a run validates its input, immutable goal contract, current capability catalog,
and worst-case budget before any task is created. Missing input or unsupported
capability returns a typed result. The experiment starts the common execution
runtime and stores `execution_run_uid`.

Experiment plans pin an exact certified simulator policy by `(policy_uid,
revision)`. Admission resolves that revision and persists its immutable binding
and component snapshot on the run and trials. Agent-loop simulator turns then
re-resolve that exact revision, verify the persisted snapshot, and send the
policy-owned provider, model, decoding controls, prompt, context contract, and
strict response schema through the production provider gateway. Typed simulator
decisions and the policy binding become part of terminal evidence. An
`execution_run` target does not use the simulator and skips this lookup.

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
records a tenant action review and the target session continues. Execution-run
experiment status mirrors the linked run. A `Review` node waits for its exact
tenant decision, while a task-local `NeedsInput` waits for its declared
audience; neither creates an unrelated session turn.

## Learning Boundary

Experiment-derived improvements must go through `learning_candidates`; they
must never auto-promote skills. Experiment execution records run
metadata and links to sessions, execution runs, artifact revisions, and score
runs. The explicit `Experiments/propose_improvements` operation may create a
candidate from completed experiment evidence, but review, evaluation, and
promotion remain separate explicit steps.

## Local Deterministic Commands

Use nextest for deterministic behavior-lab checks:

```bash
cargo nextest run -p moa-artifacts --all-targets --locked
cargo nextest run -p moa-experiments --all-targets --locked
cargo nextest run -p moa-orchestrator --test experiment_service --test experiment_trial_run_e2e --locked
cargo nextest run -p moa-edge --lib --locked
```

The `experiment_trial_run_e2e` target compiles in the default lane; its local
Restate E2E tests are ignored. To run those ignored tests, start the local
Postgres/OpenFGA/Restate dependencies first and compile the orchestrator with
provider overrides:

```bash
cargo nextest run -p moa-orchestrator \
  --test experiment_trial_run_e2e \
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

The billed simulator lane is ignored by default and requires
`MOA_RUN_LIVE_E2E=1`, `MOA_RUN_LIVE_PROVIDER_TESTS=1`, a positive
`MOA_BEHAVIOR_LAB_BUDGET_USD`, and a supported provider credential. It registers
and certifies a policy for the selected live model, admits a plan using that
exact revision, executes a real multi-turn trial through Restate and the
production provider gateway, and verifies the persisted trial and score.

Run the complete clean live lane with:

```bash
./scripts/run-clean-e2e.sh --live --providers --long-eval --behavior-lab-live
```

Do not put live or billed experiment checks in the default test lane.

## Authorization

| Surface | Required authorization |
|---|---|
| Platform regression eval harness | CI or explicitly enabled operator-run `xtask`/live lanes; it is not tenant-authorized product traffic |
| `Experiments/generate_plan`, `run`, `cancel`, `propose_improvements` | `Tenant:Operator`, which includes tenant admins and workspace admins |
| `Experiments/status`, `list`, `list_plans`, `trials`, `trial_status`, `scores`, `compare` | `Tenant:Operator`, which includes tenant admins and workspace admins |
| `Experiments/run_agent_revision_simulation`, `compare_agent_revision_simulation` | `Tenant:Operator`, which includes tenant admins and workspace admins |
| direct edge `GET /v1/analytics/catalog` | `Tenant:Operator` |
| direct edge `POST /v1/analytics/query` | `Tenant:Operator` |
| direct edge `lineage/explain`, `query`, `verify` | tenant-authorized direct edge read |

MCP is a thin adapter over product/default typed services such as `Experiments`,
direct edge analytics/lineage reads, and `Skills`. It exposes no regression-eval
tools. MCP forwards through the same DTOs and authorization boundaries instead
of owning experiment, analytics, lineage, or learning domain logic.

# Live Behavior Experiments

_Boundary between regression evals, production-path experiments, and analytics._

## Choose The Surface

Use a regression eval when the question is "did this behavior regress under a
controlled scenario?" Regression evals live in `moa-eval` and the hosted
`Eval` service. They use datasets, transcripts, scripted providers, replay, and
budget gates so CI and nightly jobs can compare runs deterministically.

Use a live behavior experiment when the question is "what happens when this
variant uses real MOA execution paths?" Live experiments live in
`moa-experiments` and the hosted `Experiments` service. They store run metadata
in `moa.experiment_run`, then admit the target through existing production
paths: `Session`/`TurnExecution` for agent loops and `WorkflowRuntime` for
artifact-backed workflow runs.

Do not use live experiments as a shortcut for regression coverage. If a live
experiment reveals a repeatable failure, turn the finding into a regression
eval or a `learning_candidates` proposal with enough evidence to reproduce it.

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
  score run IDs, then reuses the same workspace-scoped score comparison helper
  as hosted evals.

## Approvals

Live behavior experiments do not auto-approve tools. Agent-loop experiments
follow normal session approval behavior and can surface `waiting_approval` when
the underlying session waits for a human decision. Workflow experiment status
mirrors the linked artifact workflow run when a workflow run has been attached.

## Learning Boundary

Experiment-derived improvements must go through `learning_candidates`; they
must never auto-promote skills or workflows. Current experiment execution only
records run metadata and links to sessions, workflow runs, artifact revisions,
and score runs. A future proposal writer may create a candidate from experiment
evidence, but review, evaluation, and promotion remain separate explicit steps.

## Deterministic Providers

Use deterministic provider overrides for service E2E and regression-style
checks that exercise live experiment plumbing:

```bash
cargo test -p moa-orchestrator --test experiment_agent_loop_e2e \
  --features integration,provider-overrides -- --ignored
```

The test harness sets `MOA_PROVIDERS_OVERRIDE=scripted:<fixture>` or
`MOA_PROVIDERS_OVERRIDE=mock:<seed>` around the orchestrator process. Production
builds do not compile provider overrides, and the override is blocked in
production environments.

## Live And Billed Gates

Live Restate experiment tests are ignored by default because they need
Postgres, OpenFGA, Restate, and sometimes provider credentials. Run the clean
live lane only with an explicit opt-in:

```bash
MOA_RUN_LIVE_E2E=1 make e2e-clean-live
```

Provider-backed or long-running billed checks remain separate:

```bash
MOA_RUN_LIVE_E2E=1 MOA_RUN_LIVE_PROVIDER_TESTS=1 make test-provider-e2e
MOA_RUN_LIVE_E2E=1 ./scripts/run-clean-e2e.sh --live --long-eval
```

Do not put live or billed experiment checks in the default test lane.

## Authorization

| Surface | Required authorization |
|---|---|
| `Eval` suites, plan, run, status, datasets list, replay, scores, compare | `Workspace:Member` |
| `Eval` dataset registration | `Workspace:Editor` |
| `Experiments/run` and `Experiments/cancel` | `Workspace:Editor` |
| `Experiments/status`, `list`, `scores`, `compare` | `Workspace:Member` |
| `Analytics/session_stats` | `Session:Participant` |
| `Analytics/workspace_stats`, `cache_stats`, `experiment_stats`, `session_search` | `Workspace:Member` |
| `Analytics/tool_stats` with `workspace_id` | `Workspace:Member` |
| `Analytics/tool_stats` without `workspace_id` | Service identity plus `Tenant:Admin` |
| `Analytics/learning_candidates` with `workspace_id` | `Workspace:Editor` |
| `Analytics/learning_candidates` without `workspace_id` | `Tenant:Admin` |
| `LineageAdmin/explain`, `query`, `verify` | `Workspace:Member` |
| `LineageAdmin/export`, `erase` | `Workspace:Admin` |

Future MCP support is a transport adapter over these typed services. It must
forward through the same DTOs and authorization boundaries instead of owning
eval, experiment, analytics, lineage, learning, or workflow domain logic.

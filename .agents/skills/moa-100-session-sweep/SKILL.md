---
name: moa-100-session-sweep
description: Run and baseline MOA's live 100-session realistic persona sweep. Use when Codex needs to evaluate coordinator/worker behavior, skill activation, worker delegation, worker fan-in, final-response quality, durable errors, or regressions across the standard 100-session MOA persona suite. The sweep is billed and requires explicit authorization plus a budget.
---

# MOA 100-Session Sweep

## Case Source (canonical)

The 100 persona cases live in a versioned, hashed fixture:

- `.agents/skills/moa-100-session-sweep/fixtures/cases.v1.json`
- `.agents/skills/moa-100-session-sweep/fixtures/cases.v1.sha256`

Markdown sweep reports are **outputs**, never case input. The fixture is the only
input the runner reads. Every run and every baseline records the fixture's
`content_sha256`; **baselines are comparable only across runs with the same case
content hash.**

Schema (`schema_version: 1`), enforced on every load:

| Field | Rule |
|---|---|
| envelope | `schema_version == 1`, `case_count` equals the number of cases, exactly 100 cases, `content_sha256` matches the canonical serialization of `cases` |
| `id` | `S###`, unique, contiguous, and in order: exactly `S001..S100` |
| `persona` | non-empty string |
| `scenario` | integer equal to the numeric part of `id` |
| `expected_skills` | list of non-empty, unique strings (may be empty) |
| `expected_worker`, `interrupt`, `cancel` | strict booleans |
| `request` | whitespace-normalized string, >= 20 chars |

Unknown or missing per-case fields are rejected.

### Validating and editing the fixture

```bash
# unbilled: schema + contiguous ids + exact count + both hashes. This is what CI runs.
python3 .agents/skills/moa-100-session-sweep/scripts/run_100_session_sweep.py --validate-cases

# validator's own negative coverage
python3 -m unittest discover -s .agents/skills/moa-100-session-sweep/tests

# after an intentional fixture edit, refresh both hashes (validates first)
python3 .agents/skills/moa-100-session-sweep/scripts/sweep_cases.py --rehash
```

CI (`.github/workflows/ci.yml`, job `eval-fixtures`) runs the validator and the
validator tests on every PR. **Editing the fixture without refreshing both hashes
fails CI.** CI never runs the billed sweep.

Load-bearing planner-anchor request tokens (` reconcile `, ` summarize `,
` categorize `, per `project_planner_anchor_live_coverage`) are preserved in the
fixture and pinned by a test. Do not paraphrase them away.

## Workflow

1. Start from `/Users/hwuiwon/Github/moa` unless the user gives another MOA checkout.
2. Check current status before running:
   - `git status --short`
   - latest reports under `docs/engineering-discipline/live-runs/`
   - Restate/Redis/Postgres availability if the run fails early.
3. Validate the fixture first (free, instant):
   - `.../run_100_session_sweep.py --validate-cases`
4. Use the bundled runner. The sweep is billed, so it needs explicit
   authorization and a budget (see Authorization and Budget below):
   - `MOA_RUN_LIVE_100_SESSION_SWEEP=1 MOA_SWEEP_BUDGET_USD=5 MOA_SWEEP_WRITE_REPO=1 MOA_SWEEP_MODEL=gpt-5.4-mini .agents/skills/moa-100-session-sweep/scripts/run_100_session_sweep.py`
5. For a focused lane, set `MOA_SWEEP_IDS`. Focused lanes skip the canary and can
   never write the baseline (they are not 100 attempted cases):
   - `MOA_RUN_LIVE_100_SESSION_SWEEP=1 MOA_SWEEP_BUDGET_USD=1 MOA_SWEEP_IDS=S002,S004,S008 .agents/skills/moa-100-session-sweep/scripts/run_100_session_sweep.py`
6. After the run, report:
   - case fixture content hash
   - canary outcomes
   - sessions attempted (and any skipped for budget)
   - aggregate outcomes
   - failure tags (see Failure Tags below)
   - expected-worker coverage
   - total `WorkerSpawned` and `WorkerNotificationDelivered`
   - skill evidence coverage
   - durable error events
   - budget: forecast, spent, remaining
   - cost cents (total and max per session)
   - model turns (total and max per session)
   - report path and run directory

## Authorization and Budget

The billed run refuses to start unless **all** of these hold, and it names every
missing requirement instead of silently doing nothing:

- `MOA_RUN_LIVE_100_SESSION_SWEEP=1`
- a live provider credential (`MOA_ANTHROPIC_API_KEY`, `MOA_OPENAI_API_KEY`, or
  `MOA_GOOGLE_API_KEY`), `MOA_DATABASE_URL`, and a `.env.fga` file
- `MOA_SWEEP_BUDGET_USD` set to a positive finite number that covers the pre-computed
  forecast for the whole run, canary included

Forecast is `MOA_SWEEP_COST_PER_CASE_USD` (default `0.02`, deliberately above the
observed ~0.005/session on `gpt-5.4-mini`) times the number of cases plus canary
cases. **A budget below the forecast dispatches zero sessions.**

During the run a reservation ledger holds one case's forecast before each
session is dispatched and reconciles it against that session's actual provider
cost when it finishes. If actual spend consumes the remaining forecast
headroom, later cases cannot reserve and are marked `skipped`. The canary and
the full run share one ledger. This is forecast-based admission plus actual-cost
reconciliation; it is not a hard cap on provider work already in flight, which
can settle above its reservation.

## Canary

Before the 100-case job the runner executes a three-case canary
(`S001,S002,S003` — S002 exercises delegation, S003 carries the ` reconcile `
planner anchor). All three must pass before the billed 100 are dispatched, and
canary results are written to `canary.json` in the run directory. Focused
subsets (`MOA_SWEEP_IDS` / `MOA_SWEEP_LIMIT`) skip it and can never write the
committed baseline.

## Failure Tags

The analyzer classifies each session as `pass`, `partial`, or `fail` and tags failure modes. The
final-response and fan-in tags are **regression guards** that pin behavior already fixed on the
durable-subagent branch (single-owner fan-in, empty-final and raw-leak fixes); they are expected to
pass at current HEAD and should fire only on a regression.

- `F-ERROR` (fail): durable error events, a runner exception, or a non-cancel session that
  ended `failed`/`cancelled`.
- `F-EMPTY-FINAL` (fail): the last `BrainResponse` (the final reply) is missing or blank. The
  coordinator returned nothing to the user. Cancellation cases are exempt.
- `F-RAW-LEAK` (fail): the final reply reproduces a contiguous chunk (>= 120 chars, tune with
  `RAW_LEAK_MIN_CHARS`) of a worker's terminal/result payload verbatim instead of synthesizing it.
- `F-DELEGATE` (partial): an expected-worker case spawned no workers.
- `F-QUALITY` (partial): a spawned worker never delivered a terminal
  `WorkerNotificationDelivered` back to the parent (fan-in dropped a worker), regardless of
  whether delegation was expected.
- `F-LEGACY-BUNDLE` (fail): a `WorkerResultBundle` event was observed. That event was removed
  with the dynamic-execution rework — the coordinator synthesizes from terminal
  `WorkerNotificationDelivered` events. Seeing one is an immediate fan-in contract failure, never
  a flake, and is never eligible for a re-run marker. There is no expected-zero bundle counter in
  reports; the contract is enforced, not tallied.
- `F-SKILL-INJECT` (partial): an expected-skill case had no persisted segment skill evidence.

## Re-run Candidates (flaky signatures)

A single-session `fail` whose evidence matches a known flaky, non-regression signature is flagged
with a `rerun_candidate` marker (surfaced in the summary count, the Non-Pass list, and Session
Notes). These are audit-confirmed live-provider flakes that pass on a focused re-run; ~1-2% of full
sweeps hit one. The marker is triage guidance only — the session is **NOT** auto-passed and still
counts as a fail. Before treating a marked fail as a regression, re-run that session id via
`MOA_SWEEP_IDS`. Signatures: `stale-worker-timeout` (a stale worker hangs fan-in until timeout),
`canary-session_search-false-positive` (canary guardrail trips on `session_search`),
`loop-detector-memory_remember-false-positive` (tool-loop detector trips on repeated
`memory_remember`), `turn-cap-memory-store-pacing` (RESOLVED 2026-07-18: root cause was the runner not passing
`MOA_PII_SERVICE_URL` to the ephemeral orchestrator — the privacy classifier abstained, every
memory write failed closed, and the model burned its turn budget retrying; the runner now wires
the sidecar and `memory_remember` is batch-capable, so this signature firing again means a real
regression, not a flake).

## Runner Requirements

The runner expects the same local live-test setup used by prior MOA sweeps:

- Restate ingress at `MOA_RESTATE_INGRESS_URL` or `http://127.0.0.1:10010`
- Restate admin at `MOA_RESTATE_ADMIN_URL` or `http://127.0.0.1:10011`
- Redis at `MOA_SWEEP_REDIS_URL` or `redis://127.0.0.1:10051/0`
- `MOA_DATABASE_URL` pointing at a Postgres instance with a `moa_test_template_%` database
- `.env.fga` containing OpenFGA connection settings
- a live provider key for the chosen model
- `psql` and `cargo` available on PATH

The runner creates an isolated database from the latest `moa_test_template_%`,
builds `moa-orchestrator-bin` with `provider-overrides`, imports the
standard seven-skill tenant pack, runs all selected sessions, writes a report,
and drops the isolated database unless `MOA_SWEEP_KEEP_DB=1` is set.

## Output Files

Every run writes a temp report, summary, logs, and per-session JSON under the printed run directory.

The committed repo baseline report
(`docs/engineering-discipline/live-runs/<date>-moa-100-persona-baseline.md`) is written **only** when
`MOA_SWEEP_WRITE_REPO=1` is set explicitly, the run is not focused, the exact
canonical canary passed, the run reached `attempted == 100`, and no case produced
an `F-ERROR` runner-error outcome. These conditions are enforced by the runner,
so a focused lane, a budget-truncated run, or a crash can never clobber the
committed baseline. The summary JSON records `baseline_written`.

Useful env overrides:

- `MOA_RUN_LIVE_100_SESSION_SWEEP=1`: required to dispatch any billed session
- `MOA_SWEEP_BUDGET_USD`: required positive finite USD budget for the run
- `MOA_SWEEP_COST_PER_CASE_USD`: positive finite per-case forecast used for the pre-run budget check, default `0.02`
- `MOA_SWEEP_CASE_SOURCE`: alternate case fixture path (must satisfy the same schema and hashes)
- `MOA_SWEEP_REPORT_REPO`: exact repo report path to write
- `MOA_SWEEP_WRITE_REPO=1`: also overwrite the committed repo baseline report (default off: temp run directory only)
- `MOA_SWEEP_CONCURRENCY`: default `4`
- `MOA_SWEEP_SESSION_TIMEOUT_S`: default `260`
- `MOA_SWEEP_MAX_TURNS`: default `6`
- `MOA_SWEEP_LIMIT`: run first N cases
- `MOA_SWEEP_IDS`: comma-separated session ids for focused lanes
- `MOA_SWEEP_MODEL`: pinned model, usually `gpt-5.4-mini`
- `MOA_SWEEP_REDIS_URL`: runtime-cache Redis URL, default `redis://127.0.0.1:10051/0`
- `MOA_SWEEP_PROVIDER_MAX_IN_FLIGHT`: per-credential provider in-flight budget for the
  sweep orchestrator, default `64`. Chat calls are bounded per provider credential by
  default (16 unless configured), which a full sweep can saturate; the runner passes this
  through as `MOA_OPENAI_MAX_CONCURRENT_REQUESTS` so the budget never shapes outcomes.
  The runner also forces `MOA_PROVIDERS_CONCURRENCY_SCOPE=local` so the sweep never
  shares Redis lease budgets with the long-running compose orchestrator.

## Runtime-Behavior Notes (defaults changed 2026-07-10)

Interpretation guidance for sweeps at or after the 2026-07-10 defaults changes:

- **Non-builtin (MCP) tools default to admin review.** Sweep personas use builtin tools
  only, so current cases are unaffected — but any future persona that registers an MCP
  tool will stall on an approval unless the case seeds an operator allow rule or handles
  the review flow. Do not read such a stall as a delegation/fan-in regression.
- **Skill distillation now requires 8+ tool calls per segment** (was 5). This gates
  skill *learning*, not skill *activation* — `F-SKILL-INJECT` reads persisted
  `skills_activated` segment evidence and is unaffected. Expect fewer
  distillation proposals from short sweep sessions; that is intended, not a regression.
- **Provider chat concurrency is bounded per credential by default.** The runner sizes
  the sweep budget via `MOA_SWEEP_PROVIDER_MAX_IN_FLIGHT` (see above). If a sweep shows
  clustered latency spikes or rate-limit failovers, check that override before suspecting
  a provider or orchestrator regression.

## Baseline Rules

Treat the repo report as the current baseline only when all of these hold:

- the summary JSON reports `attempted=100` and `baseline_written=true`;
- the run was not focused;
- the exact canonical canary passed;
- no case produced an `F-ERROR` runner-error outcome;
- the baseline's case `content_sha256` matches the fixture you are comparing against.

Runs seeded from a different case content hash are **not comparable**. The
2026-07-08 fixture recovery started a new baseline; pre-2026-07-08 reports are not
comparable to it.

Do not treat focused lanes as baselines. Focused lanes are for quick regression
checks and can never write the baseline.

If the run fails before session execution, preserve the run directory and report
the setup failure instead of editing baseline reports by hand.

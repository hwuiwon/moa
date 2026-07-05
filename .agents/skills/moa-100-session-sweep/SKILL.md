---
name: moa-100-session-sweep
description: Run and baseline MOA's live 100-session realistic persona sweep. Use when Codex needs to evaluate coordinator/worker behavior, skill activation, worker delegation, WorkerResultBundle fan-in, final-response quality, durable errors, or regressions across the standard 100-session MOA persona suite.
---

# MOA 100-Session Sweep

## Workflow

1. Start from `/Users/hwuiwon/Github/moa` unless the user gives another MOA checkout.
2. Check current status before running:
   - `git status --short`
   - latest reports under `docs/engineering-discipline/live-runs/`
   - Restate/Redis/Postgres availability if the run fails early.
3. Use the bundled runner:
   - `scripts/run_100_session_sweep.py`
4. Run the full baseline unless the user asks for a focused lane:
   - `MOA_SWEEP_WRITE_REPO=1 MOA_SWEEP_MODEL=gpt-5.4-mini .agents/skills/moa-100-session-sweep/scripts/run_100_session_sweep.py`
5. For a focused lane, set `MOA_SWEEP_IDS` (the repo baseline is not written unless you add `MOA_SWEEP_WRITE_REPO=1`):
   - `MOA_SWEEP_IDS=S002,S004,S008 .agents/skills/moa-100-session-sweep/scripts/run_100_session_sweep.py`
6. After the run, report:
   - aggregate outcomes
   - failure tags (see Failure Tags below)
   - expected-worker coverage
   - total `WorkerSpawned`, `WorkerNotificationDelivered`, and `WorkerResultBundle`
   - skill evidence coverage
   - durable error events
   - cost cents (total and max per session)
   - model turns (total and max per session)
   - report path and run directory

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
- `F-QUALITY` (partial): workers were spawned but the run emitted **no** `WorkerResultBundle`,
  regardless of whether delegation was expected. Bundled results are **not** compared against total
  `WorkerSpawned` — the model may spawn extra workers beyond the auto-delegation run, and each
  bundle already carries one result per tracked run worker, so that comparison is a false positive.
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
`memory_remember`).

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
`MOA_SWEEP_WRITE_REPO=1` is set explicitly (as the full-baseline command does). This is off by
default so a focused lane run can never clobber the committed baseline.

Useful env overrides:

- `MOA_SWEEP_CASE_SOURCE`: source report to parse the 100 persona cases from
- `MOA_SWEEP_REPORT_REPO`: exact repo report path to write
- `MOA_SWEEP_WRITE_REPO=1`: also overwrite the committed repo baseline report (default off: temp run directory only)
- `MOA_SWEEP_CONCURRENCY`: default `4`
- `MOA_SWEEP_SESSION_TIMEOUT_S`: default `260`
- `MOA_SWEEP_MAX_TURNS`: default `6`
- `MOA_SWEEP_LIMIT`: run first N parsed cases
- `MOA_SWEEP_IDS`: comma-separated session ids for focused lanes
- `MOA_SWEEP_MODEL`: pinned model, usually `gpt-5.4-mini`
- `MOA_SWEEP_REDIS_URL`: runtime-cache Redis URL, default `redis://127.0.0.1:10051/0`

## Baseline Rules

Treat the repo report as the current baseline only after the full 100-session run
finishes and the summary JSON reports `attempted=100`.

Do not treat focused lanes as baselines. Focused lanes are for quick regression
checks and should normally use `MOA_SWEEP_WRITE_REPO=0`.

If the run fails before session execution, preserve the run directory and report
the setup failure instead of editing baseline reports by hand.

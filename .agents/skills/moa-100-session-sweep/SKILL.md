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
5. For a focused lane, set `MOA_SWEEP_IDS`:
   - `MOA_SWEEP_IDS=S002,S004,S008 MOA_SWEEP_WRITE_REPO=0 .agents/skills/moa-100-session-sweep/scripts/run_100_session_sweep.py`
6. After the run, report:
   - aggregate outcomes
   - failure tags
   - expected-worker coverage
   - total `WorkerSpawned`, `WorkerNotificationDelivered`, and `WorkerResultBundle`
   - skill evidence coverage
   - durable error events
   - cost cents
   - report path and run directory

## Runner Requirements

The runner expects the same local live-test setup used by prior MOA sweeps:

- Restate ingress at `MOA_RESTATE_INGRESS_URL` or `http://127.0.0.1:10010`
- Restate admin at `MOA_RESTATE_ADMIN_URL` or `http://127.0.0.1:10011`
- Redis at `redis://127.0.0.1:10051/0`
- `MOA_DATABASE_URL` pointing at a Postgres instance with a `moa_test_template_%` database
- `.env.fga` containing OpenFGA connection settings
- a live provider key for the chosen model
- `psql` and `cargo` available on PATH

The runner creates an isolated database from the latest `moa_test_template_%`,
builds `moa-orchestrator-bin` with `provider-overrides,redis`, imports the
standard seven-skill tenant pack, runs all selected sessions, writes a report,
and drops the isolated database unless `MOA_SWEEP_KEEP_DB=1` is set.

## Output Files

By default a full run writes:

- Repo report: `docs/engineering-discipline/live-runs/<date>-moa-100-persona-baseline.md`
- Temp report, summary, logs, and per-session JSON under the printed run directory

Useful env overrides:

- `MOA_SWEEP_CASE_SOURCE`: source report to parse the 100 persona cases from
- `MOA_SWEEP_REPORT_REPO`: exact repo report path to write
- `MOA_SWEEP_WRITE_REPO=0`: keep output only under the temp run directory
- `MOA_SWEEP_CONCURRENCY`: default `4`
- `MOA_SWEEP_SESSION_TIMEOUT_S`: default `260`
- `MOA_SWEEP_MAX_TURNS`: default `6`
- `MOA_SWEEP_LIMIT`: run first N parsed cases
- `MOA_SWEEP_IDS`: comma-separated session ids for focused lanes
- `MOA_SWEEP_MODEL`: pinned model, usually `gpt-5.4-mini`

## Baseline Rules

Treat the repo report as the current baseline only after the full 100-session run
finishes and the summary JSON reports `attempted=100`.

Do not treat focused lanes as baselines. Focused lanes are for quick regression
checks and should normally use `MOA_SWEEP_WRITE_REPO=0`.

If the run fails before session execution, preserve the run directory and report
the setup failure instead of editing baseline reports by hand.

# moa-scoring

Shared score-run storage and score summary queries. Provides the
tenant-scoped helpers services use against the `analytics.score_run` and
`analytics.scores` tables: ensuring a score-run parent row exists,
summarizing one run, and comparing numeric scores between two runs.

## Entry points

- `ensure_score_run_parent` — inserts a score-run parent or validates that an
  existing parent matches the requested scope and source
- `score_summaries_for_tenant` — per-name summary rows (count, numeric mean or
  boolean true-rate) for one score run
- `compare_score_runs_for_tenant` — numeric mean deltas between a baseline run
  and a new run
- `ScoreRunRef`, `ScoreCompareRef`, `ScoreSummary`, `ScoreCompare` —
  request/response DTOs

## Rules

- Every query is scoped by `StoragePartitionId::for_tenant`, so summaries and
  comparisons never cross tenants.
- Score-run parents are idempotent per `run_id`; reusing an existing `run_id`
  with a different scope or source fails with `Error::ScoreRunMismatch`.

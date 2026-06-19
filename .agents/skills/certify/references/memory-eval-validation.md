# Memory Eval Validation

Use this reference when validating memory-retrieval baselines, query-rewrite gating, ranking changes, graph-leg tuning, or live memory-eval lanes.

## Baseline Artifacts

- Durable PR baseline: `docs/eval/baselines/memory-retrieval-pr-baseline.json`.
- Fresh local reports usually live under `target/memory-eval/`.
- Compare a fresh report to a previous report with `MOA_EVAL_PREVIOUS_MEMORY_REPORT`.
- Checked-in live notes belong under `docs/eval/live/`; do not rely on transient `target/` output as the only evidence of a live run.

## PR Validation Shape

1. Verify the local compose state before assuming Postgres is running.
2. Bring up Postgres only when the eval path needs it: `docker compose up -d postgres`.
3. Set `MOA_DATABASE_URL` for the local Postgres instance.
4. Generate or reuse the corpus required by the eval command.
5. Run the memory retrieval eval and write a fresh report under `target/memory-eval/`.
6. Run `check-eval-budgets --suite memory_retrieval` with `MOA_EVAL_PREVIOUS_MEMORY_REPORT` when comparing against an existing baseline.
7. Stop compose services with `docker compose down` when they were started for the task.

## Required Comparisons

- Ranking or retrieval-policy changes need a paired baseline compare, not only compile/test success.
- Quality-prior changes need a negative control such as inverted priors when practical.
- Query-rewrite gating changes must report retrieval quality, latency, and cost impact.
- Preserve downstream `QueryRewriteResult` semantics: it feeds segmentation and advisory fields, not only vector-search text.
- If a report shows zero graph-tagged candidates or `per_leg_recall.graph == 0.0`, inspect graph expansion/fusion before score tuning.

## Live Lane

- Live/billed runs must be opt-in and credential-gated.
- Record provider provenance, estimated cost, and live-lane scope in the report or checked-in note.
- Local live reranker runs may hit provider trial-key rate or monthly caps; use the nightly lane as the authoritative full-scale path when local credentials are constrained.

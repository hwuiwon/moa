# Memory Eval Pipeline

The memory eval pipeline gates memory architecture changes with a hermetic
report before shipping new retrieval machinery. It decomposes failures into
three ordered questions:

1. Did ingestion preserve the expected facts? Track `ingestion_coverage`.
   For production-shaped source text, also track `scope_match_rate` and
   `extraction_precision`.
2. Did retrieval surface the preserved facts? Track `recall_at_4`,
   `recall_at_25`, `mrr`, `ndcg_at_4`, `zero_recall_rate`, and
   `per_leg_recall`.
3. Did the answer use retrieved evidence correctly? Track
   `answer_faithfulness`, `abstention_correctness`, and
   `temporal_as_of_accuracy`.

Do not skip the order. A faithful answer cannot recover a fact that ingestion
lost, and recall metrics cannot explain a hallucinated answer unless ingestion
coverage is already known.

## Report Contract

`run-memory-retrieval-eval` writes a JSON report with the loaded corpus
manifest, retrieval cutoffs, gold-resolution details, per-probe results,
bootstrap intervals, cross-user leak probe ids, and `RetrievalMetrics`.

The metric surface is:

- `ingestion_coverage`
- `scope_match_rate`
- `extraction_precision`
- `recall_at_4`
- `recall_at_25`
- `mrr`
- `ndcg_at_4`
- `zero_recall_rate`
- `p50_retrieval_latency_ms`
- `p95_retrieval_latency_ms`
- `answer_faithfulness`
- `abstention_correctness`
- `cross_user_leak_count`
- `pii_unredacted_count`
- `pii_redaction_rate`
- `temporal_as_of_accuracy`
- `temporal_parse_rate`
- `temporal_parse_mismatch_count`
- `per_leg_recall`

`per_leg_recall` attributes expected fact recall to graph, vector, and lexical
retrieval legs. Use it to localize the first bottleneck before proposing a new
ranking or indexing feature.

Superseded graph writes delete the old pgvector row, so historical hits for
superseded facts must be carried by lexical lookup plus graph traversal and
hydration. Vector retrieval can only serve historical rows whose embedding rows
were retained.

`scope_match_rate` is the fraction of resolved gold facts whose stored scope
matches the ledger scope; `mixed` counts as a mismatch. `extraction_precision`
is the fraction of stored `Fact` nodes in the eval workspaces that map back to a
ledger fact, including superseded nodes in both numerator and denominator.

## Eval Layering

`moa-eval::kernel` is the suite-agnostic layer for retrieval stats, core
metrics, and paired report comparison. Kernel modules must not import
`memory_eval` or any future suite module; suites import the kernel instead.
The guard test `kernel_sources_never_import_memory_eval` enforces that rule.

`RetrievalCoreMetrics` contains the universal retrieval metrics: recall, MRR,
nDCG, zero-recall rate, per-leg recall, latency, cross-user leak count, and
unredacted-PII count. The memory suite flattens those core fields into
`RetrievalMetrics` and keeps memory-specific fields such as ingestion,
temporal, faithfulness, abstention, and redaction metrics as extensions.
Promote the kernel to a separate crate only when a second suite needs it.

`CachedHybridRetriever` caches final ranked hits. Its key includes scope, query
text and embedding fingerprint, cutoff, reranker flag, temporal filter, ranking
reference time, and a stable ranking fingerprint made from the ranking config
plus `RANKING_PIPELINE_VERSION`.

## PR Hermetic Check

PR checks use the `pr` corpus profile with marked transcripts. The run is
deterministic and uses cached embedding fixtures; it must not call live
providers or billed embedding APIs. It does require a local Postgres URL,
usually from the MOA compose stack:

```bash
export MOA_TEST_POSTGRES_URL=postgres://moa_owner:dev@127.0.0.1:10040/moa
cargo run -p xtask -- generate-memory-eval-corpus --profile pr --seed 1 --seed 2 --seed 3 --output target/memory-eval/pr
cargo run -p xtask -- run-memory-retrieval-eval --corpus target/memory-eval/pr --output target/memory-eval/report.json
cargo run -p xtask -- check-eval-budgets --suite memory_retrieval --max-regression-pct 5 --memory-eval-report target/memory-eval/report.json
```

The budget gate treats `cross_user_leak_count != 0` and
`pii_unredacted_count != 0` as hard blockers. These failures block regardless
of improvements to recall, MRR, or nDCG.

PR runs use the deterministic `FeatureV1` ranking mode by default. Pass
`--ranking legacy` to `run-memory-retrieval-eval` only for A/B comparison. In
PR runs without Cohere credentials, the reranker is `Noop`, so the post-rerank
top 4 equals the pre-rerank top 4.

## Transcript styles

Corpus size and transcript realism are separate axes. `--profile pr|full`
controls size, and `--transcript-style marked|natural` controls source-turn
rendering.

`marked` is the CI-gated style. It keeps the legacy `Fact:` and scope markers
so deterministic retrieval and ranking changes can be validated without
depending on a better extractor.

`natural` is an observed profile. It renders conversational deterministic
sentences, includes at least one distractor turn per session, and contains no
`Fact:`, `workspace shared`, or `user private` markers. The heuristic extractor
is expected to lose facts, drift scope, and extract spurious distractors here;
that is the signal this profile exists to preserve. Natural reports enforce
hard blockers (`cross_user_leak_count == 0` and `pii_unredacted_count == 0`)
but do not gate quality metrics until the Section 2 extraction work can move
them.

Corpus realism v2 also expands PR-profile multi-hop probes from 6 to 30 using
cross-session `depends_on`/`owned_by` fact pairs. Recall@4 is mechanically lower
than prompt-02 baselines because multi-hop probes require both supporting facts
inside the final window.

## Paired Comparison

Ranking-affecting changes ship only when `compare-eval-reports` shows a paired
`recall_at_4` delta with a cluster-bootstrap confidence interval excluding 0
and a Benjamini-Hochberg-adjusted McNemar p-value below 0.05. MRR and nDCG
should move in the same direction, and `recall_at_25` should stay unchanged
unless the change intentionally alters candidate generation.

Run legacy and candidate reports on the same corpus, then compare them:

```bash
cargo run -p xtask -- run-memory-retrieval-eval --corpus target/memory-eval/pr --ranking legacy --output target/memory-eval/legacy.json
cargo run -p xtask -- run-memory-retrieval-eval --corpus target/memory-eval/pr --ranking feature_v1 --output target/memory-eval/feature_v1.json
cargo run -p xtask -- compare-eval-reports --baseline target/memory-eval/legacy.json --candidate target/memory-eval/feature_v1.json
```

The comparison refuses cross-corpus inputs with exit code 2. It checks corpus
identity, seeds, final cutoff, and exact probe-id set equality before computing
paired statistics.

For the `FeatureV1` default selected in this change, the PR-profile sweep picked
`overlap = 0.35` and `subject_match = 0.5`: it improved `recall_at_4` by +0.098
over legacy on the current corpus with CI `[+0.059,+0.142]` and adjusted p-value
`0.010`.

## Nightly And Manual Scale Check

Nightly and manual scale checks use the `full` corpus profile. Use the same
retrieval and budget commands, but keep the artifacts separate from PR output:

```bash
export MOA_TEST_POSTGRES_URL=postgres://moa_owner:dev@127.0.0.1:10040/moa
cargo run -p xtask -- generate-memory-eval-corpus --profile full --seed 1 --seed 2 --seed 3 --output target/memory-eval/full
cargo run -p xtask -- run-memory-retrieval-eval --corpus target/memory-eval/full --output target/memory-eval/full-report.json
cargo run -p xtask -- check-eval-budgets --suite memory_retrieval --max-regression-pct 5 --memory-eval-report target/memory-eval/full-report.json
```

To compare against a previous report, set:

```bash
export MOA_EVAL_PREVIOUS_MEMORY_REPORT=target/memory-eval/previous-report.json
```

Baseline regression gates compare `retrieval.recall_at_4`, `retrieval.mrr`,
and `retrieval.ndcg_at_4` against `MOA_EVAL_PREVIOUS_MEMORY_REPORT`.

The current PR-profile baseline is checked in at:

```bash
docs/eval/baselines/memory-retrieval-pr-baseline.json
```

The observed natural PR-profile baseline is checked in at:

```bash
docs/eval/baselines/memory-retrieval-pr-natural-baseline.json
```

Use it when evaluating an implementation candidate:

```bash
export MOA_TEST_POSTGRES_URL=postgres://moa_owner:dev@127.0.0.1:10040/moa
export MOA_EVAL_PREVIOUS_MEMORY_REPORT=docs/eval/baselines/memory-retrieval-pr-baseline.json
cargo run -p xtask -- generate-memory-eval-corpus --profile pr --seed 1 --seed 2 --seed 3 --output target/memory-eval/pr-candidate
cargo run -p xtask -- run-memory-retrieval-eval --corpus target/memory-eval/pr-candidate --output target/memory-eval/candidate-report.json
cargo run -p xtask -- check-eval-budgets --suite memory_retrieval --max-regression-pct 5 --memory-eval-report target/memory-eval/candidate-report.json
```

## Architecture Gate

Do not ship BM25, PPR, profile digest injection, consolidation redesign,
outcome-weighting, or adaptive gating until the memory eval report shows the
bottleneck that feature addresses.

Examples:

- Add BM25 only after `per_leg_recall.lexical` or lexical miss analysis shows a
  lexical bottleneck that graph and vector recall do not cover.
- Add PPR only after graph leg attribution shows relationship expansion or
  ranking is the bottleneck.
- Add profile digest injection only after ingestion and retrieval succeed but
  answer faithfulness or preference-application probes still fail from missing
  profile context.
- Redesign consolidation only after gold resolution or temporal probes show
  facts were merged, superseded, or expired incorrectly.
- Add outcome-weighting only after top-k candidates contain the right facts but
  `mrr` or `ndcg_at_4` shows ranking quality is the bottleneck.
- Add adaptive gating only after the report shows predictable over-retrieval or
  under-retrieval patterns that fixed cutoffs cannot address.
- Add LLM extraction or scope classification only after the natural profile's
  `ingestion_coverage` and `scope_match_rate` show the marked extractor signal
  no longer represents production transcripts.
- Add entity-resolution upgrades only after `extraction_precision` and graph-leg
  attribution show spurious facts or unresolved entity links are the bottleneck.

The report decides the next architecture step. A feature without a named report
bottleneck stays out of the shipping path.

## Triage Order

When the budget gate fails, triage in this order:

1. Hard blockers: fix any `cross_user_leak_count != 0` or
   `pii_unredacted_count != 0` before reading quality metrics.
2. Ingestion: inspect `ingestion_coverage`, `scope_match_rate`,
   `extraction_precision`, and the gold-resolution section.
3. Retrieval: inspect `recall_at_4`, `recall_at_25`, `mrr`, `ndcg_at_4`,
   `zero_recall_rate`, and `per_leg_recall`.
4. Answer behavior: inspect `answer_faithfulness`,
   `abstention_correctness`, `pii_redaction_rate`, and
   `temporal_as_of_accuracy`. For temporal failures, read
   `temporal_parse_rate` first: a low parse rate is a planner bug, while a high
   parse rate with low accuracy is a retrieval-leg or validity-window bug.
5. Baseline drift: when `MOA_EVAL_PREVIOUS_MEMORY_REPORT` is set, inspect
   regression failures for `retrieval.recall_at_4`, `retrieval.mrr`, and
   `retrieval.ndcg_at_4`.

See [Evaluation](../16-evaluation.md) for the broader long-conversation eval
contract and [moa-eval](../../crates/moa-eval/README.md) for crate-level entry
points.

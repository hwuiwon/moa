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
- `scope_match_rate_user`
- `scope_match_rate_workspace`
- `extraction_precision`
- `entity_fragmentation`
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

The graph leg is a seeded expansion leg. Retrieval runs vector and lexical
first, then expands from planner NER seeds plus the top phase-one fused hits.
Expansion applies the same as-of validity window as the other legs, scores
paths with hop decay and edge weights, treats `Entity` rows as conduits, and
feeds surviving `Fact` ids back into normal RRF fusion. The current PR baseline
keeps the aggregate-best 250ms graph budget profile; graph-leg recall is still
the tuning target for the next pass.

Superseded graph writes delete the old pgvector row, so historical hits for
superseded facts must be carried by lexical lookup plus graph traversal and
hydration. Vector retrieval can only serve historical rows whose embedding rows
were retained.

`scope_match_rate` is the fraction of resolved gold facts whose stored scope
matches the ledger scope; `mixed` counts as a mismatch.
`scope_match_rate_user` and `scope_match_rate_workspace` split that tally by
expected ledger scope so a one-sided privacy or recall drift cannot hide behind
the overall rate. `extraction_precision` is the fraction of stored `Fact` nodes
in the eval workspaces that map back to a ledger fact, including superseded
nodes in both numerator and denominator. `entity_fragmentation` is active
`Entity` nodes over distinct normalized ledger subject/object mentions in their
storage scopes. A value near 1.0 means mentions are neither fragmented nor
over-merged; the natural lane gates a floor of 0.90 and reviews values above
1.30 as fragmentation.

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

The memory eval runner still uses the production planner, cache, and hybrid
retriever, but its default ranking config is time-neutral. Recorded extraction
replay also forces exact pgvector scans and writes `0` latency values so two
hermetic runs can produce byte-identical reports.

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

`marked` is the deterministic heuristic regression style. It keeps the legacy
`Fact:` and scope markers so retrieval and ranking changes can be validated
without depending on model extraction.

`natural` is the recorded extractor and merge-verifier gate. It renders
conversational deterministic sentences, includes at least one distractor turn
per session, and contains no `Fact:`, `workspace shared`, or `user private`
markers. CI replays committed extraction and merge fixtures with no provider
credentials and gates `ingestion_coverage >= 0.85`,
`scope_match_rate >= 0.90`, `extraction_precision >= 0.80`,
`entity_fragmentation >= 0.90`, plus the hard blockers
`cross_user_leak_count == 0` and `pii_unredacted_count == 0`.

## Recorded Extraction Lane

The natural profile can run with model-backed extraction without making CI or
PR checks call a live provider. Recording is a deliberate, billed maintainer
step; replay is hermetic.

Record fixtures after changing the natural corpus, the extraction prompt, or
the extractor model:

```bash
cargo run -p xtask -- record-memory-extractions --corpus target/memory-eval/pr-natural
```

The default fixture path is:

```bash
crates/moa-eval/fixtures/memory/extractions-<corpus_id>-v2.jsonl
```

Replay with no credentials required:

```bash
env -u COHERE_API_KEY cargo run -p xtask -- run-memory-retrieval-eval \
  --corpus target/memory-eval/pr-natural \
  --extractor recorded \
  --output target/memory-eval/natural-recorded.json
```

Extraction fixtures are keyed by the SHA-256 hex hash of the raw chunk text the
extractor saw. The file name and every record carry the extraction prompt
version. If the prompt changes, bump `EXTRACTION_PROMPT_VERSION`, record a new
file, and commit the fixture diff. The kernel `FixtureStore` rejects version
mismatches and missing keys; missing-key errors include the exact recording
command to regenerate the fixture set.

The natural CI lane runs:

```bash
cargo run -p xtask -- generate-memory-eval-corpus --profile pr --transcript-style natural --seed 1 --seed 2 --seed 3 --output target/memory-eval/pr-natural
env -u COHERE_API_KEY cargo run -p xtask -- run-memory-retrieval-eval --corpus target/memory-eval/pr-natural --extractor recorded --output target/memory-eval/natural-recorded.json
cargo run -p xtask -- check-eval-budgets --suite memory_retrieval \
  --memory-eval-report target/memory-eval/natural-recorded.json \
  --min-metric ingestion_coverage=0.85 \
  --min-metric scope_match_rate=0.90 \
  --min-metric extraction_precision=0.80 \
  --min-metric entity_fragmentation=0.90
```

Corpus realism v2 also expands PR-profile multi-hop probes from 6 to 30 using
cross-session `depends_on`/`owned_by` fact pairs. Recall@4 is mechanically lower
than prompt-02 baselines because multi-hop probes require both supporting facts
inside the final window.

## Recorded Merge Lane

Entity-resolution v2 embeds normalized entity mentions at creation and uses
same-scope KNN as a bounded candidate block before asking the merge verifier.
The verifier has a live Cohere-backed implementation for recording and a
recorded implementation over the kernel `FixtureStore` for CI replay. The
default fixture path is:

```bash
crates/moa-eval/fixtures/memory/merges-<corpus_id>-v1.jsonl
```

Record merge fixtures after changing entity blocking, the merge prompt, the
natural corpus, or recorded extraction fixtures:

```bash
cargo run -p xtask -- record-memory-merges --corpus target/memory-eval/pr-natural
```

Replay uses `--extractor recorded`; the runner resolves both extraction and
merge fixture paths from the corpus id unless `--extractions` or `--merges`
override them. The current deterministic PR-natural corpus does not generate
any verifier calls at the 0.80 KNN threshold, so its v1 merge fixture is an
empty but versioned JSONL file. Live embedding geometry may produce a different
candidate set; prompt 08's live lane must report `entity_fragmentation` so that
threshold can be recalibrated against real vectors.

Object edges now carry deterministic typed labels for dependency and ownership
predicates (`DEPENDS_ON`, `OWNED_BY`); subject attachment edges remain
`RELATES_TO`. The ranking pipeline version is `3` because typed edges and graph
candidate weighting change cacheable candidate pools.

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

The gated recorded natural PR-profile baseline is checked in at:

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
- Add entity-resolution upgrades only after `extraction_precision`, entity
  fragmentation diagnostics, and natural-profile graph-leg attribution show
  spurious facts or unresolved entity links are the bottleneck.

The report decides the next architecture step. A feature without a named report
bottleneck stays out of the shipping path.

## Triage Order

When the budget gate fails, triage in this order:

1. Hard blockers: fix any `cross_user_leak_count != 0` or
   `pii_unredacted_count != 0` before reading quality metrics.
2. Ingestion: inspect `ingestion_coverage`, `scope_match_rate`,
   `scope_match_rate_user`, `scope_match_rate_workspace`,
   `extraction_precision`, and the gold-resolution section.
3. Retrieval: inspect `recall_at_4`, `recall_at_25`, `mrr`, `ndcg_at_4`,
   `zero_recall_rate`, and `per_leg_recall`.
   Low graph recall with healthy vector and lexical recall now points at edge
   topology, entity resolution, or graph expansion latency rather than missing
   phase-one seeds.
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

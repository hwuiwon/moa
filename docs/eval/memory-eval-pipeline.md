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
bootstrap intervals, cross-user leak probe ids, optional provider `cost`,
optional provider provenance, optional `consolidation`, and
`RetrievalMetrics`.

The metric surface is:

- `ingestion_coverage`
- `scope_match_rate`
- `scope_match_rate_contact`
- `scope_match_rate_tenant`
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
- `preference_context_rate`
- `per_leg_recall`

Top-level query rewrite fields report the retrieval rewrite policy and call
accounting: `query_rewrite_policy`, `query_rewrite_call_count`,
`query_rewrite_skip_count`, `query_rewrite_call_rate`,
`query_rewrite_p50_latency_ms`, `query_rewrite_p95_latency_ms`,
`query_rewrite_input_tokens`, `query_rewrite_output_tokens`,
`query_rewrite_est_usd`, `retrieval_plus_rewrite_p95_latency_ms`, and
`query_rewrite_by_class`.

`per_leg_recall` attributes expected fact recall to graph, vector, and lexical
retrieval legs. Use it to localize the first bottleneck before proposing a new
ranking or indexing feature.

When present, `consolidation` is the `moa-memory-lifecycle`
`ConsolidationOutcome`: `merged`, `decayed`, `at_floor`,
`contradiction_supersessions`, `entity_embeddings_backfilled`,
`aliases_promoted`, `duplicates_remaining`, `digests_rebuilt`, and
`digests_skipped_fresh`. Old reports omit this section and still deserialize
with `consolidation: null`.

`preference_context_rate` is a memory-suite extension for standing digests. For
each `preference_application` probe, the runner checks whether the expected
preference appears in the union of that user's digest content and the probe's
final top-4 candidate facts. The existing `preference_application` retrieval
slice remains retrieval-only; the gap between the two numbers shows how much
standing context is helping without hiding ranking misses.

Quality-weighted ranking is also a memory-suite extension. Ledger facts can
carry synthetic `prior_uses` and `prior_successes`; the runner resolves those
facts to graph UIDs and seeds `moa.node_index.quality_score` before probing.
The ranker treats the neutral default `0.5` as zero contribution, so reports
with unset priors preserve the pre-quality ordering except for the ranking
pipeline version.

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
`scope_match_rate_contact` and `scope_match_rate_tenant` split that tally by
expected ledger scope so a one-sided privacy or recall drift cannot hide behind
the overall rate. `extraction_precision` is the fraction of stored `Fact` nodes
in the eval tenants that map back to a ledger fact, including superseded
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
plus `RANKING_PIPELINE_VERSION` 8. The fingerprinted config shape excludes the
ranking-mode switch while retaining the stemmed token features, first-person
scope boost, graph-rescue weight, and OR lexical leg behavior.

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
export MOA_DATABASE_URL=postgres://moa_owner:dev@127.0.0.1:10040/moa
cargo run -p xtask -- generate-memory-eval-corpus --profile pr --seed 1 --seed 2 --seed 3 --output target/memory-eval/pr
cargo run -p xtask -- run-memory-retrieval-eval --corpus target/memory-eval/pr --output target/memory-eval/report.json
cargo run -p xtask -- check-eval-budgets --suite memory_retrieval --max-regression-pct 5 --memory-eval-report target/memory-eval/report.json
```

The budget gate treats `cross_user_leak_count != 0` and
`pii_unredacted_count != 0` as hard blockers. These failures block regardless
of improvements to recall, MRR, or nDCG.

PR runs use deterministic post-hydration ranking. In PR runs without Cohere
credentials, the reranker is `Noop`, so the post-rerank top 4 equals the
pre-rerank top 4.

## Transcript styles

Corpus size and transcript realism are separate axes. `--profile pr|full`
controls size, and `--transcript-style marked|natural` controls source-turn
rendering.

`marked` is the deterministic heuristic regression style. It keeps explicit
`Fact:` and scope markers so retrieval and ranking changes can be validated
without depending on model extraction.

`natural` is the recorded extractor and merge-verifier gate. It renders
conversational deterministic sentences, includes at least one distractor turn
per session, and contains no `Fact:`, `tenant shared`, or `contact private`
markers. CI replays committed extraction and merge fixtures with no provider
credentials and gates `ingestion_coverage >= 0.85`,
`scope_match_rate >= 0.90`, `scope_match_rate_contact >= 0.90`,
`scope_match_rate_tenant >= 0.90`, `extraction_precision >= 0.80`,
`entity_fragmentation >= 0.90`, plus the hard blockers
`cross_user_leak_count == 0` and `pii_unredacted_count == 0`.

Recurring single-valued facts (`response_style`, `contact_email`,
`private_repository`, `require_runbook`) are linked across era sessions into
one supersession chain: later eras close the previous era's validity window,
probes that target a superseded era are rewritten into explicit
`as of YYYY-MM-DD` queries inside that era's window, and every linked probe
blocks its family's other eras. Without this the corpus issued identical
present-tense queries that expected three different gold facts, capping those
slices near one third. Supersession and contradiction marked transcripts also
carry the scope marker derived from the ledger scope; omitting it stored
tenant facts as contact scope and made them invisible to other contacts' probes.

The PR natural corpus also plants verbatim restatement pairs in later sessions
for the same user. These restating facts carry `restates: <canonical fact_id>`
and probes target only the canonical fact. They exist to prove exact
`fact_hash` consolidation without changing recall targets.

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
env -u MOA_COHERE_API_KEY cargo run -p xtask -- run-memory-retrieval-eval \
  --corpus target/memory-eval/pr-natural \
  --extractor recorded \
  --output target/memory-eval/natural-recorded.json
```

To exercise lifecycle consolidation in the same hermetic lane, add
`--consolidate` after gold resolution and before probes:

```bash
env -u MOA_COHERE_API_KEY cargo run -p xtask -- run-memory-retrieval-eval \
  --corpus target/memory-eval/pr-natural \
  --extractor recorded \
  --consolidate \
  --output target/memory-eval/natural-recorded-consolidated.json
```

The runner invokes the storage-internal
`moa_memory_lifecycle::consolidate_tenant` function once per eval tenant
with the corpus reference time and the eval embedding provider, then runs a
second pass in the same invocation. The second pass must report no mutating
work; otherwise the run fails as non-idempotent. For every
`restates` pair, the runner verifies via the gold UID map and a direct active
row count that exactly one node remains active.

To build standing profile digests and score preference context, add
`--digests`. When combined with `--consolidate`, the digest rebuild runs as the
consolidation digest step. Without `--consolidate`, the eval calls
`moa_memory_lifecycle::rebuild_digests` directly after gold resolution:

```bash
env -u MOA_COHERE_API_KEY cargo run -p xtask -- run-memory-retrieval-eval \
  --corpus target/memory-eval/pr-natural \
  --extractor recorded \
  --consolidate \
  --digests \
  --output target/memory-eval/natural-recorded-digests.json
```

To exercise outcome-weighted ranking, run the same corpus twice: once with the
quality term zeroed and once with the default weight. The default quality
weight is 0.6, calibrated against the paired gate below. The generated natural
corpus seeds expected facts with high synthetic priors and same-subject lexical
colliders with low synthetic priors:

```bash
env -u MOA_COHERE_API_KEY cargo run -p xtask -- run-memory-retrieval-eval \
  --corpus target/memory-eval/pr-natural \
  --extractor recorded \
  --quality-weight 0.0 \
  --output target/memory-eval/q0.json
env -u MOA_COHERE_API_KEY cargo run -p xtask -- run-memory-retrieval-eval \
  --corpus target/memory-eval/pr-natural \
  --extractor recorded \
  --output target/memory-eval/q.json
cargo run -p xtask -- compare-eval-reports \
  --baseline target/memory-eval/q0.json \
  --candidate target/memory-eval/q.json
```

Then run the inverted-prior negative control. It must regress MRR with a
confidence interval excluding zero, proving the quality term has enough weight
to matter rather than acting as a no-op:

```bash
env -u MOA_COHERE_API_KEY cargo run -p xtask -- run-memory-retrieval-eval \
  --corpus target/memory-eval/pr-natural \
  --extractor recorded \
  --invert-quality-priors \
  --output target/memory-eval/qinv.json
cargo run -p xtask -- compare-eval-reports \
  --baseline target/memory-eval/q.json \
  --candidate target/memory-eval/qinv.json
```

This gate proves mechanism and weight magnitude, not production prior quality.
The priors are synthetic by design. Production quality is owned by live lineage
data and task-segment outcomes after `memory.retrieval.lineage_enabled` is
enabled. Quality scores are an outcome-gated ranking prior: no lineage row
contributes until it maps to a persisted task-segment outcome, and only
`resolved` outcomes count as successes. The scorer does not create learning
candidates, publish skills, cache retrieval results, or autonomously promote
memory; it updates only the sidecar `moa.node_index.quality_score` field.

Lineage capture is dark by default. When
`memory.retrieval.lineage_enabled = true`, retrieval writes best-effort rows to
`moa.retrieval_lineage` with tenant storage key, contact, session, turn
sequence, UID, rank, and timestamp; write errors trace and never fail
retrieval. The dark scoring job is manual:

```bash
cargo run -p xtask -- compute-memory-quality-scores --tenant-id <tenant-uuid>
```

It applies Beta(1,1) smoothing, `(1 + successes) / (2 + uses)`, over lineage
rows joined to persisted task segments with non-null outcomes. Lineage for
pending segments or turns that cannot be mapped to an outcome is skipped rather
than counted as a failed use. If no outcome source is present, the job logs a
structured warning, reports `skipped_no_outcome_source`, and writes nothing.
It is not scheduled; production enablement also needs a lineage pruning policy.

Extraction fixtures are keyed by the SHA-256 hex hash of the raw chunk text the
extractor saw. The file name and every record carry the extraction prompt
version. If the prompt changes, bump `EXTRACTION_PROMPT_VERSION`, record a new
file, and commit the fixture diff. The kernel `FixtureStore` rejects version
mismatches and missing keys; missing-key errors include the exact recording
command to regenerate the fixture set.

The natural CI lane runs:

```bash
cargo run -p xtask -- generate-memory-eval-corpus --profile pr --transcript-style natural --seed 1 --seed 2 --seed 3 --output target/memory-eval/pr-natural
env -u MOA_COHERE_API_KEY cargo run -p xtask -- run-memory-retrieval-eval --corpus target/memory-eval/pr-natural --extractor recorded --output target/memory-eval/natural-recorded.json
cargo run -p xtask -- check-eval-budgets --suite memory_retrieval \
  --memory-eval-report target/memory-eval/natural-recorded.json \
  --min-metric ingestion_coverage=0.85 \
  --min-metric scope_match_rate=0.90 \
  --min-metric scope_match_rate_contact=0.90 \
  --min-metric scope_match_rate_tenant=0.90 \
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
`RELATES_TO`. The ranking pipeline version is `8` because typed edges and graph
candidate weighting change cacheable candidate pools.

## Nightly Live Lane

PR and natural recorded lanes are hermetic by design: they use deterministic
sha256 embeddings, recorded extraction and merge fixtures, and a `Noop`
reranker. The live lane measures the assumptions those fixtures cannot cover:
real Cohere embedding geometry, live extraction and merge-verifier calls, and
Cohere reranking.

Run the PR preset with no live providers:

```bash
env -u MOA_COHERE_API_KEY cargo run -p xtask -- run-memory-retrieval-eval \
  --corpus target/memory-eval/pr-natural \
  --lane pr \
  --extractor recorded \
  --output target/memory-eval/hermetic.json
```

Run the live PR-natural pair with bounded spend:

```bash
cargo run -p xtask -- run-memory-retrieval-eval \
  --corpus target/memory-eval/pr-natural \
  --lane live \
  --reranker off \
  --budget-usd 5 \
  --output target/memory-eval/live.json

cargo run -p xtask -- run-memory-retrieval-eval \
  --corpus target/memory-eval/pr-natural \
  --lane live \
  --reranker on \
  --budget-usd 5 \
  --output target/memory-eval/live-rerank.json
```

`--lane live` requires `MOA_COHERE_API_KEY`. It ignores hermetic embedding fixtures
entirely and refuses fixture flags such as `--extractor`, `--extractions`, and
`--merges`. `--budget-usd` is live-only; PR runs reject it so accidental billing
flags do not become inert configuration.

The live lane writes `cost` and `providers` sections into the report. Cost is an
estimate, not an invoice: wrappers count embed input tokens, chat input/output
tokens, and rerank searches, using provider-reported counts where available and
falling back to chars/4 estimates otherwise. The estimate constants are
date-stamped in `moa-eval::kernel::cost` and are used only to enforce the eval
ceiling. The report's `providers` block records the embedding model and
version, extractor model and prompt version, merge prompt version, reranker
model, and lane.

Budget enforcement checks after ingestion and every 10 probes. If the estimate
exceeds the ceiling, the runner writes a partial report marked
`aborted_over_budget: true` and exits nonzero. `check-eval-budgets` also treats
that marker as a hard blocker when reading an uploaded partial report.

Nightly live runs are informational. They fail only for hard blockers
(`cross_user_leak_count != 0`, `pii_unredacted_count != 0`) or budget aborts.
They do not regression-gate recall, MRR, nDCG, scope, or fragmentation because
provider behavior can drift outside a code change. The nightly workflow pairs
the same PR-natural corpus three ways:

```bash
cargo run -p xtask -- compare-eval-reports \
  --baseline target/memory-eval/hermetic.json \
  --candidate target/memory-eval/live.json

cargo run -p xtask -- compare-eval-reports \
  --baseline target/memory-eval/live.json \
  --candidate target/memory-eval/live-rerank.json
```

Read vector-leg and entity-fragmentation deltas as calibration data. A
`per_leg_recall.vector` difference is expected and is the point of the lane:
the hermetic PR geometry is a deterministic stand-in, not a quality claim about
Cohere vectors. The prompt-07 `0.80` entity-blocking threshold was tuned against
pseudo-embeddings; live `entity_fragmentation` is the number that should drive
any threshold change. Reranker A/B deltas decide whether the live reranker earns
its latency and spend.

## Paired Comparison

Ranking-affecting changes ship only when `compare-eval-reports` shows a paired
`recall_at_4` delta with a cluster-bootstrap confidence interval excluding 0
and a Benjamini-Hochberg-adjusted McNemar p-value below 0.05. MRR and nDCG
should move in the same direction, and `recall_at_25` should stay unchanged
unless the change intentionally alters candidate generation.

Run baseline and candidate reports on the same corpus, then compare them:

```bash
cargo run -p xtask -- run-memory-retrieval-eval --corpus target/memory-eval/pr --output target/memory-eval/baseline.json
cargo run -p xtask -- run-memory-retrieval-eval --corpus target/memory-eval/pr --ranking-subject-match 0.6 --output target/memory-eval/candidate.json
cargo run -p xtask -- compare-eval-reports --baseline target/memory-eval/baseline.json --candidate target/memory-eval/candidate.json
```

Query rewrite policy A/B uses the same corpus and retrieval metrics. PR runs are
hermetic: `always` and `gated` use deterministic rewrite fixtures and report
rewrite call accounting without calling a provider. The generated PR corpus
includes exact-identifier negative controls and treats temporal-as-of probes as
explicit temporal retrieval, because the temporal parser owns the as-of instant.

```bash
cargo run -p xtask -- run-memory-retrieval-eval --corpus target/memory-eval/pr --output target/memory-eval/rewrite-off.json --rewrite-policy off
cargo run -p xtask -- run-memory-retrieval-eval --corpus target/memory-eval/pr --output target/memory-eval/rewrite-always.json --rewrite-policy always
cargo run -p xtask -- run-memory-retrieval-eval --corpus target/memory-eval/pr --output target/memory-eval/rewrite-gated.json --rewrite-policy gated
MOA_EVAL_PREVIOUS_MEMORY_REPORT=target/memory-eval/rewrite-always.json cargo run -p xtask -- check-eval-budgets --suite memory_retrieval --max-regression-pct 5 --memory-eval-report target/memory-eval/rewrite-gated.json
```

The rewrite budget gate compares `gated` against `always` for recall@4,
recall@25, MRR, nDCG@4, rewrite-inclusive p95 latency, and at least 50% fewer
rewrite calls. It also requires exact-identifier controls to be present and to
skip rewriting in gated mode.

The comparison refuses cross-corpus inputs with exit code 2. It checks corpus
identity, seeds, final cutoff, and exact probe-id set equality before computing
paired statistics.

For the current deterministic scorer, the PR-profile sweep picked `overlap =
0.35` and `subject_match = 0.5`: it improved `recall_at_4` by +0.098 over the
previous baseline on the current corpus with CI `[+0.059,+0.142]` and adjusted
p-value `0.010`.

With stemmed tokens, first-person scope boost, graph-rescue weight, and the OR
lexical leg on the era-linked marked corpus, the deterministic scorer improved
`recall_at_4` by +0.187 over the previous baseline with CI `[+0.154,+0.239]`
and adjusted p-value `0.000`. The remaining hermetic top-4 gap is concentrated
in multi-hop probes, where both chain facts must fit the final window; that
slice is reranker territory and should be judged in the live lane.

## Nightly And Manual Scale Check

Nightly and manual scale checks use the `full` corpus profile. Use the same
retrieval and budget commands, but keep the artifacts separate from PR output:

```bash
export MOA_DATABASE_URL=postgres://moa_owner:dev@127.0.0.1:10040/moa
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
export MOA_DATABASE_URL=postgres://moa_owner:dev@127.0.0.1:10040/moa
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
  The hermetic quality gate only validates the ranking mechanism; production
  prior quality belongs to the live lane after lineage and task outcomes exist.
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
   `scope_match_rate_contact`, `scope_match_rate_tenant`,
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

# Golden Retrieval Set

_Graded offline golden set for gating retrieval changes before live sweeps._

## Purpose

Every retrieval-affecting change (ranking, fusion weights, graph policy,
reranker, router, decomposition) gates on the offline golden retrieval eval
before it may request the live 100-session sweep. The offline lane is
deterministic, hermetic (`_db_memory`, recorded embeddings), and cheap enough
to run in CI on every PR; the live sweep validates end-to-end behavior after
the offline scorecard is green.

## Format

The golden set is the memory-eval corpus probe format
(`crates/moa-eval/src/memory_eval/corpus.rs`). One probe is one labeled query:

- `probe_id` — stable identifier; never reuse an id for changed semantics.
- `probe_type` — the slice key (`point_recall`, `multi_hop`, `temporal_as_of`,
  `preference_application`, `abstention`, `cross_user_isolation`, ...).
- `query` — the retrieval query as the user would phrase it.
- `expected_fact_ids` — ledger facts a correct retrieval returns.
- `expected_fact_grades` — graded 0-3 relevance per expected fact:
  - `3` — directly answers the query; the answer is wrong without it.
  - `2` — required supporting evidence (for example one hop of a multi-hop
    chain).
  - `1` — useful context; helps but does not decide the answer.
  - `0` — labeled explicitly irrelevant (kept for regression tracking).
  Facts absent from the map default to grade 3, so binary-labeled probes stay
  valid. Grades exist because binary labels hide ranking regressions a
  reranker or router introduces: retrieving everything in the wrong order
  scores perfect binary recall.
- `blocked_fact_ids` — facts that must never be returned (isolation probes).

## Metrics And Gating

The offline lane reports, per run and per probe-type slice:

- `graded_ndcg_at_10` — headline graded ranking quality.
- `recall_at_4` / `recall_at_25` — final-window and candidate-window recall.
- `mrr`, `zero_recall_rate`.
- `per_probe_type.<slice>.<metric>.{mean,std_error,count}` — sliced values.
- Cluster-bootstrap confidence intervals for headline metrics (`bootstrap`).

Gate on the slice a change's mechanism can move, not only the global mean:
routing and decomposition help some intents and hurt others by construction,
so a global mean can hide a per-intent regression (Simpson's paradox). Floors
are enforced through the memory budget gate (`--min-metric`), e.g.:

```bash
cargo xtask check-eval-budgets --suite memory_retrieval \
  --min-metric per_probe_type.multi_hop.graded_ndcg_at_10.mean=0.60 \
  --min-metric per_probe_type.point_recall.recall_at_4.mean=0.85
```

Never compare single-run point estimates across branches: a delta inside the
bootstrap interval (or within ~2 standard errors on a slice) is noise, not a
win. Report intervals alongside means in scorecards.

## Held-Out Acceptance Split

Seeds `101`, `102`, and `103` are the **held-out set**: they are never used
during self-improvement proposal iteration (mining, drafting, or tuning). They
exist only to confirm no regression at acceptance time — a proposal must win on
data it did not iterate against, which is the post's reward-hacking defense. The
same three seeds are pinned in code as
`moa_eval::memory_eval::HELD_OUT_GOLDEN_SEEDS` so tooling and this document
cannot drift. `generate-memory-eval-corpus --held-out` is the only CLI path
that may select them; ordinary explicit `--seed` generation rejects any
intersection with the reservation. The protected lane uses the deterministic
marked PR profile, cached embeddings, heuristic extraction, deterministic
merge verification, and noop reranking. It is a retrieval/privacy acceptance
mechanism, not a reader or generated-answer quality measurement.

The manual workflow is `.github/workflows/memory-eval-held-out.yml`. It accepts
only `workflow_dispatch` from `refs/heads/main`, declares the
`memory-eval-held-out` GitHub environment, compares against the dedicated
held-out baseline with an explicit five-percent regression ceiling, and uploads
the corpus, report, and paired comparison. Before enabling the workflow, a
repository administrator must configure that environment with required
reviewers, prevent self-review, and restrict deployment branches to `main`.
Those controls live in GitHub repository settings and cannot be enforced by the
checked-in workflow alone.

## Labeling Protocol

1. **Mine queries from real usage**: replayed persona-sweep sessions and (with
   consent controls) production `retrieval_lineage` rows with poor downstream
   citation scores are the primary sources. Synthetic generator probes
   (`memory_eval/generator/`) fill coverage holes, but at least one slice per
   probe type should come from realistic phrasing.
2. **Label relevance against the corpus, not the answer**: graders see the
   query and candidate facts, assign 0-3 per the scale above, and record a one
   line rationale for grades 1-2 (those are the ambiguous ones).
3. **De-bias any LLM-assisted labeling or judging**:
   - randomize pairwise order and evaluate both orders; an order-dependent
     verdict is a tie;
   - length-match comparison buckets;
   - pin and version the judge model; re-baseline when it changes;
   - never judge Claude-family output with a Claude-family judge
     (self-preference alone can fabricate a double-digit win).
4. **Version the set**: golden-set changes land as normal PRs; a change that
   relabels existing probes must re-baseline every floor in the same PR. Never
   silently edit labels to make a retrieval change pass.

## Lanes

- Offline metrics unit lane: `moa-eval::memory_eval_metrics_offline`.
- Hermetic end-to-end lane: memory-eval `_db_memory` runner tests (recorded
  embeddings + isolated schemas), see
  [Memory Eval Pipeline](memory-eval-pipeline.md).
- Live lanes remain flag-gated (`MOA_RUN_LIVE_*`) and billed; they are the
  post-gate confirmation, not the PR gate.

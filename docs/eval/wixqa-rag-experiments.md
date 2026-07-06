# WixQA RAG Experiments

This file is the running scorecard for WixQA retrieval experiments in MOA. Append new runs here as JSON reports are produced so quality, latency, and cost decisions stay tied to measured results.

## Priorities

1. Retrieval quality: prefer higher article-level recall, MRR, and NDCG.
2. Latency: keep the default path acceptable for realtime conversations.
3. Price: avoid paid rerank or extra embedding work unless the measured quality gain justifies it.

## Current Evaluation Setup

- Dataset: WixQA `simulated` split.
- Main working subset: 200 questions over 1000 selected articles.
- Gold target: article-level retrieval. A query is correct when a retrieved chunk maps back to a gold `article_id`.
- Current chunking baseline: target 700 tokens, max 1000 tokens, min 120 tokens.
- Current 1000-article cache key: `wixqa:1b58d9e0355e7f5fe7be66da4aa369b6513cad26429671590ef97ee9cb022baf`.
- Cached chunks for the current 1000-article baseline rerun: 1152.
- Chunk300 cache key: `wixqa:96f504a50d744dd8e308c27844e06265e86b80ee78144694a9ac02b33d244faf`, with 1541 cached chunks.
- Embedding model: `cohere:embed-v4.0`, 1024 dimensions.
- Gemini 2 cache key: `wixqa:88b84d07d448ee9d88d0a79d2e6ac0f9f1c0424d21699b7b95e4cbe0f686c1b5`, with `gemini:gemini-embedding-2` at 1024 dimensions and 1152 cached chunks.
- Rerank model when enabled: `cohere:rerank-v4.0-fast`.
- Vector backend under test: Turbopuffer, with pgvector kept as a local comparison path.

## Decision Baseline

The current default recommendation is:

- Turbopuffer vector-first retrieval.
- `top_k=25`.
- Graph expansion disabled for tenant knowledge retrieval.
- Turbopuffer BM25 disabled on the vector-first tenant knowledge path.
- Cohere rerank disabled by default.
- Optional WixQA candidate: weak-repeat k50 fallback when higher recall is preferred over the lowest p95.

Rationale: rerank improves rank quality but is too slow and expensive for default realtime use; BM25 variants rescued too little recall or hurt rank quality enough to stay out of the default path. Weak-repeat k50 fallback is the best measured no-rerank recall lever. Chunk300 plus weak-repeat fallback is the best measured query-time score, but it requires rebuilding the graph/vector cache and increases indexed chunks by about 34%.

## Main Scorecard

| Date | Run | Report | Backend / mode | K | Rerank | Recall@K | Hit@K | MRR | NDCG@K | Query embed p95 ms | Retrieval p95 ms | Total p95 ms | Est. USD | Verdict |
|---|---|---|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 2026-07-05 | pgvector comparison | `.moa/wixqa/reports/simulated-200q-1000a-pgvector-no-graph.json` | pgvector, no graph | 10 | no | 0.9525 | 0.955 | 0.7351 | 0.7814 | 212 | 36 | 231 | 0.0795 | Good local comparison; lower recall than Turbopuffer k25. |
| 2026-07-05 | vector baseline k25 | `.moa/wixqa/reports/simulated-200q-1000a-tp-no-graph-k25.json` | Turbopuffer vector, no graph | 25 | no | 0.9750 | 0.975 | 0.7359 | 0.7870 | 141 | 136 | 248 | 0.0795 | Best measured default baseline before vector-first gating landed. |
| 2026-07-05 | vector baseline k50 | `.moa/wixqa/reports/simulated-200q-1000a-tp-no-graph-k50.json` | Turbopuffer vector, no graph | 50 | no | 0.9850 | 0.985 | 0.7364 | 0.7893 | 136 | 113 | 242 | 0.0795 | Higher recall with similar historical latency; candidate for fallback tier. |
| 2026-07-05 | vector-first k25 | `.moa/wixqa/reports/simulated-200q-1000a-tp-vectorfirst-k25.json` | Turbopuffer vector-first, no graph, BM25 gated off | 25 | no | 0.9750 | 0.975 | 0.7342 | 0.7858 | 1084 | 154 | 1174 | 0.0795 | Recommended default; total p95 was dominated by Cohere query-embedding spikes in this run. |
| 2026-07-05 | vector-first k25 current rerun | `.moa/wixqa/reports/simulated-200q-1000a-tp-vectorfirst-k25-current-rerun.json` | Turbopuffer vector-first, no graph, BM25 gated off | 25 | no | 0.9750 | 0.975 | 0.7342 | 0.7858 | 155 | 128 | 261 | 0.0795 | Fresh same-code baseline for fallback comparisons. |
| 2026-07-05 | weak-repeat fallback k50 | `.moa/wixqa/reports/simulated-200q-1000a-tp-vectorfirst-k25-weak-repeat-fallback-k50-rerun.json` | k25 first pass; rerun k50 when top-10 source articles are all unique | mixed 25/50 | no | 0.9950 | 0.995 | 0.7352 | 0.7903 | 167 | 238 | 366 | 0.0795 | Best recall so far without rerank; rescued 4/5 k25 misses with no breaks, but reran 113/200 queries and raises p95 retrieval. Candidate fallback tier. |
| 2026-07-05 | dynamic candidates k50 | `.moa/wixqa/reports/simulated-200q-1000a-tp-no-graph-k50-dynamic-candidates-cache1b58.json` | Turbopuffer vector, dynamic candidate limits | 50 | no | 0.9900 | 0.990 | 0.7302 | 0.7853 | 294 | 230 | 476 | 0.0795 | Best recall so far, but worse MRR/NDCG and slower retrieval. Useful for fallback tier, not default. |
| 2026-07-05 | chunk-text rerank | `.moa/wixqa/reports/simulated-200q-1000a-tp-no-graph-k25-rerank-chunktext-cache1b58.json` | Turbopuffer vector, rerank sees hydrated chunk text | 25 | yes | 0.9825 | 0.985 | 0.7714 | 0.8162 | 785 | 707 | 1368 | 0.4795 | Best rank quality, but too slow and about 6x default cost. Candidate for selective rerank only. |
| 2026-07-05 | BM25 additive | `.moa/wixqa/reports/simulated-200q-1000a-tp-bm25-content-k25.json` | Turbopuffer vector + BM25 additive | 25 | no | 0.9133 | 0.925 | 0.6089 | 0.6701 | 1092 | 152 | 1211 | 0.0795 | Rejected; BM25 candidate addition hurt quality badly. |
| 2026-07-05 | BM25 boost 0.10 | `.moa/wixqa/reports/simulated-200q-1000a-tp-bm25-boost010-k25.json` | Turbopuffer vector + BM25 boost-only | 25 | no | 0.9800 | 0.980 | 0.7034 | 0.7635 | 1112 | 137 | 1225 | 0.0795 | Recall improved slightly over k25, but rank quality regressed. Rejected as default. |
| 2026-07-06 | weak-repeat selective rerank | `.moa/wixqa/reports/simulated-200q-1000a-tp-vectorfirst-k25-weak-repeat-rerank.json` | k25 first pass; rerank only weak-repeat queries | 25 | selective | 0.9875 | 0.990 | 0.7601 | 0.8064 | 1681 | 719 | 2217 | 0.3055 | Good rank quality, but p95 and paid rerank calls are too high for realtime default. |
| 2026-07-06 | chunk300 k25 | `.moa/wixqa/reports/simulated-200q-1000a-tp-vectorfirst-chunk300-k25.json` | Turbopuffer vector-first, smaller chunks | 25 | no | 0.9750 | 0.975 | 0.7366 | 0.7875 | 137 | 143 | 250 | 0.0795 | Similar recall to baseline with tiny rank/latency gains; rebuild took 22.3 min and created 1541 embeddings. |
| 2026-07-06 | chunk300 weak-repeat fallback k50 | `.moa/wixqa/reports/simulated-200q-1000a-tp-vectorfirst-chunk300-k25-weak-repeat-fallback-k50.json` | chunk300; k25 first pass; rerun k50 on weak-repeat queries | mixed 25/50 | no | 0.9950 | 0.995 | 0.7374 | 0.7921 | 135 | 202 | 309 | 0.0795 | Best no-rerank query-time result; same recall as default fallback, better p95/MRR/NDCG, but requires re-chunk/reembed. |
| 2026-07-06 | Gemini 2 small smoke | `.moa/wixqa/reports/simulated-small-tp-gemini2-1024-k10.json` | Turbopuffer vector-first, Gemini 2 1024d, no graph | 10 | no | 1.0000 | 1.000 | 1.0000 | 1.0000 | 395 | 215 | 610 | 0.0134 | Smoke passed; matched small-set quality but query embedding was slower and estimated embedding cost was higher than Cohere. |
| 2026-07-06 | Gemini 2 768d attempt | `.moa/wixqa/reports/simulated-small-tp-gemini2-768-k10.json` | Turbopuffer vector-first, Gemini 2 768d, no graph | 10 | no | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | Rejected by current storage path: pgvector KNN requires 1024 dimensions in this MOA stack. |
| 2026-07-06 | Gemini 2 k10 | `.moa/wixqa/reports/simulated-200q-1000a-tp-gemini2-1024-k10-cache88b84.json` | Turbopuffer vector-first, Gemini 2 1024d, no graph | 10 | no | 0.9750 | 0.980 | 0.7918 | 0.8322 | 443 | 104 | 531 | 0.1325 | Strong ranking, but recall matches cheap Cohere k25 while costing more and running slower. Not fast-mode default. |
| 2026-07-06 | Gemini 2 k10 fallback k25 | `.moa/wixqa/reports/simulated-200q-1000a-tp-gemini2-1024-k10-weak-repeat-fallback-k25-cache88b84.json` | Gemini 2 k10 first pass; rerun k25 on weak-repeat queries | mixed 10/25 | no | 0.9925 | 0.995 | 0.7856 | 0.8311 | 408 | 196 | 569 | 0.1325 | Recovers most recall but is slower and lower-quality than direct Gemini 2 k25. Rejected. |
| 2026-07-06 | Gemini 2 k25 | `.moa/wixqa/reports/simulated-200q-1000a-tp-gemini2-1024-k25-cache88b84-rerun.json` | Turbopuffer vector-first, Gemini 2 1024d, no graph | 25 | no | 1.0000 | 1.000 | 0.7881 | 0.8358 | 433 | 200 | 559 | 0.1325 | Best measured retrieval quality without rerank. Good slow-mode candidate; too slow/costly for phone-call fast mode. |
| 2026-07-06 | Gemini 2 k25 selective rerank | `.moa/wixqa/reports/simulated-200q-1000a-tp-gemini2-1024-k25-weak-repeat-rerank-cache88b84.json` | Gemini 2 k25 first pass; rerank weak-repeat queries | 25 | selective | 0.9875 | 0.990 | 0.7636 | 0.8127 | 425 | 645 | 977 | 0.3465 | Rejected; rerank hurt Gemini 2 quality and added 107 paid rerank calls. |
| 2026-07-06 | Gemini 2 k25 graph | `.moa/wixqa/reports/simulated-200q-1000a-tp-gemini2-1024-k25-graph-cache88b84.json` | Turbopuffer vector-first, Gemini 2 1024d, graph expansion on | 25 | no | 0.9950 | 0.995 | 0.4858 | 0.6114 | 390 | 164 | 542 | 0.1325 | Rejected; graph expansion preserved broad hit rate but badly damaged ranking. |
| 2026-07-06 | Gemini 2 k25 legacy graph diagnostics | `.moa/wixqa/reports/simulated-200q-1000a-tp-gemini2-1024-k25-graph-diagnostics-cache88b84.json` | Turbopuffer vector-first, Gemini 2 1024d, legacy broad graph with diagnostics | 25 | no | 0.9950 | 0.995 | 0.4858 | 0.6114 | 381 | 166 | 487 | 0.1325 | Diagnostic baseline; 141 hurt, 12 rescue, 47 neutral. Harmful paths are dominated by broad-fallback structural paths such as `CONTAINS -> CONTAINS`. |
| 2026-07-06 | Gemini 2 k25 anchored rescue | `.moa/wixqa/reports/simulated-200q-1000a-tp-gemini2-1024-k25-anchored-rescue-cache88b84.json` | Turbopuffer vector-first, Gemini 2 1024d, AnchoredRescue graph policy | 25 | no | 1.0000 | 1.000 | 0.7881 | 0.8358 | 396 | 104 | 491 | 0.1325 | Guardrail pass; matches graph-off MRR/NDCG, with 0 hurt, 0 rescue, 200 neutral, and graph p95 0 ms. |
| 2026-07-06 | Gemini 2 k25 ArticleGraph rerun | `.moa/wixqa/reports/simulated-200q-1000a-tp-gemini2-1024-k25-article-graph-cache88b84-rerun.json` | Turbopuffer vector-first, Gemini 2 1024d, ArticleGraph policy | 25 | no | 1.0000 | 1.000 | 0.8147 | 0.8540 | 398 | 121 | 505 | 0.1325 | Accepted for balanced/slow candidate: no recall loss versus graph-off, MRR +0.0266, NDCG +0.0182, total p95 below 1s. |
| 2026-07-06 | Gemini 2 k10 graph | `.moa/wixqa/reports/simulated-200q-1000a-tp-gemini2-1024-k10-graph-cache88b84.json` | Turbopuffer vector-first, Gemini 2 1024d, graph expansion on | 10 | no | 0.9267 | 0.945 | 0.4407 | 0.5518 | 429 | 128 | 528 | 0.1325 | Rejected; graph hurts fast-mode recall and rank quality. |
| 2026-07-06 | Gemini 2 k25 graph rerank | `.moa/wixqa/reports/simulated-200q-1000a-tp-gemini2-1024-k25-graph-rerank-cache88b84.json` | Gemini 2 k25 graph expansion plus full Cohere rerank | 25 | yes | 0.9925 | 0.995 | 0.7684 | 0.8156 | 353 | 611 | 919 | 0.5325 | Rejected; rerank recovers much of graph's rank damage but is still worse than no-graph Gemini 2 k25 and costs about 4x. |

## Small-Set Smoke Runs

These runs used 10 questions over 100 articles and are useful only for quick smoke validation.

| Date | Run | Report | K | Rerank | Graph disabled | Recall@K | MRR | NDCG@K | Total p95 ms | Verdict |
|---|---|---|---:|---|---|---:|---:|---:|---:|---|
| 2026-07-05 | small baseline | `.moa/wixqa/reports/simulated-small-tp-baseline.json` | 10 | no | no | 1.000 | 0.6819 | 0.7624 | 423 | Smoke passed. |
| 2026-07-05 | small query-role no graph | `.moa/wixqa/reports/simulated-small-tp-query-role-no-graph.json` | 10 | no | yes | 1.000 | 1.0000 | 1.0000 | 389 | Smoke passed; no-graph looked best on tiny set. |
| 2026-07-05 | small rerank | `.moa/wixqa/reports/simulated-small-tp-rerank.json` | 10 | yes | no | 0.700 | 0.3593 | 0.4406 | 574 | Rejected as too noisy and worse on tiny set. |

## Excluded Or Invalid Runs

| Date | Report | Why excluded |
|---|---|---|
| 2026-07-05 | `.moa/wixqa/reports/simulated-200q-1000a-tp-no-graph-k25-dynamic-candidates.json` | Partial-cache run using only 143 cached chunks; not a valid ranking result. The harness now validates cached article coverage before `--skip-ingestion`. |
| 2026-07-05 | `.moa/wixqa/reports/simulated-200q-1000a-tp-vectorfirst-k25-weak-repeat-fallback-k50.json` | First weak-repeat fallback run had two non-fallback queries return empty hit lists while the immediate baseline rerun did not. Repeated fallback run was clean and is the scorecard entry. |

## Experiment Notes

### 2026-07-05 P0 Replay And Fallback Experiments

- Article-level aggregation replay over `.moa/wixqa/reports/simulated-200q-1000a-tp-vectorfirst-k25.json` did not improve recall. `max` was identical to first occurrence; top-N sum hurt rank quality; a weak multi-hit/title boost improved MRR only from 0.7342 to 0.7368 and NDCG from 0.7858 to 0.7877. Verdict: not worth production complexity yet.
- Miss bucket from the current k25 baseline: 5 misses. k50 expansion rescues 4; chunk-text rerank rescues 3; BM25 additive rescues 1 but breaks 11 prior hits; BM25 boost 0.10 rescues 1 and breaks 0 but lowers MRR/NDCG. Verdict: prioritize conditional k expansion before more BM25 tuning.
- Offline conditional k expansion predicted that falling back to k50 when the top-10 hits have no repeated source article would trigger on 113/200 queries, rescue 4 misses, break 0 hits, and improve recall/hit to 0.995.
- Live cached fallback rerun confirmed the signal: recall@mixed improved from 0.975 to 0.995, hit@mixed from 0.975 to 0.995, NDCG from 0.7858 to 0.7903, MRR from 0.7342 to 0.7352. Retrieval p95 increased from 128 ms to 238 ms, total p95 from 261 ms to 366 ms, with no added Cohere rerank cost.
- Selective rerank replay using the same weak-repeat trigger would rerank 113/200 queries, rescue 3 misses, break 0 hits, and produce recall 0.9875, hit 0.990, MRR 0.7601, NDCG 0.8064. Estimated rerank-only cost would be about 113 calls. Verdict: better rank quality than k fallback, but paid rerank latency/cost remains a concern.

### 2026-07-06 Follow-Up Experiments

- Live weak-repeat selective rerank confirmed the replayed rank-quality shape: recall 0.9875, hit 0.990, MRR 0.7601, NDCG 0.8064. It triggered 113/200 rerank calls and raised total p95 to 2217 ms with estimated cost 0.3055. Verdict: do not put Cohere rerank in the default realtime path.
- Chunk300 rebuilt the 1000-article cache with target 300, max 500, min 80. It created 1541 embeddings and took 1,338,909 ms to ingest. Query quality without fallback stayed tied on recall at 0.975 but nudged MRR from 0.7342 to 0.7366 and total p95 from 261 ms to 250 ms. Verdict: not worth rebuilding by itself.
- Chunk300 plus weak-repeat k50 fallback triggered only 37/200 fallback queries and reached recall/hit 0.995, MRR 0.7374, NDCG 0.7921, total p95 309 ms, with no rerank calls. Verdict: best measured no-rerank query-time profile if the one-time re-chunk/reembed migration is acceptable.
- Metadata/title replay over existing vector hits: title-overlap boost with alpha 0.05 kept recall/hit at 0.975 and improved MRR only to 0.7367 and NDCG to 0.7877. Larger title boosts hurt ranking. Verdict: not enough alone.
- Offline BM25 over selected articles was much worse than vector retrieval: pure BM25 body recall 0.7842, hit 0.805, MRR 0.4133, NDCG 0.4880; title-weighted BM25 was similar. Vector plus title-weighted BM25 at alpha 0.1 rescued one miss and reached recall/hit 0.980, MRR 0.7373, NDCG 0.7888, but still underperformed weak-repeat fallback. Verdict: keep BM25 out of default; revisit only as a highly gated rescue feature.

### 2026-07-06 Gemini Embedding 2 Experiments

- The harness now supports `--embedder-name` with `cohere:embed-v4.0` or `gemini:gemini-embedding-2` and `--embedding-dim`. Other embedder names are rejected; Gemini 001 is not selectable. Cache keys include embedder name and dimension.
- Gemini 2 at 768 dimensions failed in the current MOA memory stack because pgvector KNN requires 1024 dimensions. Until the canonical vector storage path supports variable dimensions, Gemini 2 experiments should stay at 1024d.
- Gemini 2 k25 is the best measured retrieval-quality profile: recall/hit 1.000, MRR 0.7881, NDCG 0.8358, total p95 559 ms, estimated cost 0.1325. It raises quality materially over Cohere chunk300 fallback but costs about 67% more in embedding spend and has slower query embeddings.
- Gemini 2 k10 is not a good fast-mode default. It has high MRR/NDCG but recall 0.975 and total p95 531 ms, so it is slower and costlier than Cohere k25 while matching Cohere k25 recall.
- Gemini 2 k10 with weak-repeat fallback to k25 is dominated by direct Gemini 2 k25: lower recall/MRR/NDCG and higher p95.
- Gemini 2 k25 with selective Cohere rerank is rejected: recall fell to 0.9875 and MRR/NDCG fell versus no-rerank Gemini 2 k25, while cost rose to 0.3465.
- Current mode readout: fast should remain Cohere k25 for latency/cost; balanced should use Cohere chunk300 weak-repeat fallback if low cost matters or Gemini 2 k25 if quality can dominate cost; slow should use Gemini 2 k25 until a better article-collapsed rerank beats it under the 3s cap.

### 2026-07-06 Gemini 2 Plus Graph Expansion

- Graph expansion can be used with Gemini 2 by reusing the same `gemini:gemini-embedding-2` cache and omitting `--disable-graph-expansion`; no re-embedding is required when the graph-linked chunks already exist.
- On the matched Gemini 2 k25 workload, graph expansion reduced recall from 1.0000 to 0.9950, MRR from 0.7881 to 0.4858, and NDCG from 0.8358 to 0.6114. Total p95 stayed similar at 542 ms versus 559 ms, so the problem is ranking quality rather than latency.
- Rank movement analysis for k25 graph versus k25 no-graph: 12 queries improved, 141 worsened, and 47 were unchanged. Rank-1 hits dropped from 132 to 46; rank-2/3 hits rose from 46 to 93, showing graph often pushes related but wrong articles ahead of the exact gold article.
- The graph-on k25 run had 794 hits tagged with the graph leg, all overlapping vector hits in this report. This suggests the current graph path is mostly boosting related vector candidates rather than adding missing pure graph candidates.
- Graph plus full Cohere rerank recovered much of the rank damage but still underperformed no-graph Gemini 2 k25: recall 0.9925, MRR 0.7684, NDCG 0.8156, total p95 919 ms, and estimated cost 0.5325 with 200 paid rerank calls.
- Verdict: for WixQA tenant knowledge RAG, graph expansion currently adds negative value with Gemini 2. Keep graph disabled for the fast/balanced/slow WixQA modes until we test a narrower graph policy, such as one-hop/title-exact expansion, graph as recall-only fallback, or graph candidates reranked at article level.

### 2026-07-06 Graph Policy Guardrail Implementation

- Milestone 1 added `GraphRetrievalPolicy`, request-level graph diagnostics, per-query graph-on/off comparison, and memory-eval graph harm reporting. The legacy diagnostic run produced 23,273 raw graph paths, 5,174 broad-fallback seeds, and the same 141 hurt / 12 rescue / 47 neutral split seen in replay analysis.
- The harmful path evidence shows the core failure: generic phase-one seeds traverse structural containment edges such as `CONTAINS -> CONTAINS` and graph-confirm wrong sibling chunks/articles.
- Milestone 2 changed the default graph policy from legacy broad expansion to `AnchoredRescue`. Legacy broad expansion remains selectable only by explicit `--graph-policy legacy-broad-expansion` for A/B reports.
- `AnchoredRescue` suppresses broad phase-one fallback, skips graph ranking for `ContextOnly`/`Off`, limits tenant chunk graph expansion, filters edge labels and traversal directions, requires an explicit graph evidence floor, and prevents graph-only rescue bonuses outside the legacy policy.
- The AnchoredRescue acceptance run matched graph-off quality exactly on the 200q/1,000a Gemini 2 k25 workload: recall/hit 1.000, MRR 0.7881, NDCG 0.8358. It produced 0 hurt, 0 rescue, and 200 neutral comparisons with graph p95 0 ms.
- Verdict: guardrails eliminate the WixQA degradation without adding recall yet. This is a safe default replacement, not a final quality improvement. The next quality work should be article-level graph aggregation or semantic graph extraction.

### 2026-07-06 ArticleGraph Implementation

- Milestone 3 added an explicit `ArticleGraph` ranking path behind `--graph-policy article-graph`; `AnchoredRescue` remains the default.
- The implementation groups hydrated tenant knowledge chunk candidates by `object_uid`, scores articles with max chunk score, lexical/title overlap, same-article repeat, exact-title match, typed graph evidence, adjacent chunk support, and a structural-only graph penalty, then orders final chunks article-first with adjacent same-article context support.
- WixQA reports now aggregate `graph_diagnostics.article_ranking`, including feature totals and top article rank movements. Per-query retrieval diagnostics keep the top article feature contribution rows.
- ArticleGraph tenant chunk graph traversal is guarded to one hop and still does not admit legacy broad phase-one fallback.
- The 200q/1,000a ArticleGraph acceptance rerun passed the production graph-ranking gate: recall/hit 1.000, MRR 0.8147, NDCG 0.8540, total p95 505 ms, 0 hurt / 0 rescue / 200 neutral, and estimated query-time cost unchanged at 0.1325.
- Compared with Gemini 2 k25 graph-off, ArticleGraph kept recall flat while improving MRR by 0.0266 and NDCG by 0.0182. A first ArticleGraph run had one transient empty retrieval, but the immediate full rerun was clean and is the accepted report.
- The ArticleGraph feature totals show the current gain comes from article grouping, lexical/title signal, same-article repeat, and adjacent chunk support. Typed graph evidence is still 0 on this shallow WixQA graph, so semantic graph extraction remains the next graph-specific quality milestone.
- Memory PR retrieval eval was regenerated and run with the current default graph policy as a regression guard: `target/memory-eval/reports/article-graph-current-pr.json` reached pre/post recall@4 0.881, recall@25 0.964, NDCG@4 0.835, preference context rate 1.000, p95 retrieval 83 ms, and passed the memory retrieval budget gate.

### 2026-07-06 Semantic Graph Extraction Slice

- Milestone 4 first slice adds ingestion-time Wix/support semantic extraction without query-time provider calls. The extractor emits schema-constrained entities and relations, stores confidence/provenance in graph properties, maps relations onto existing graph labels, and persists a tenant-scoped cache keyed by chunk hash, content hash, schema version, model, and prompt version.
- The graph delta now writes semantic `Entity` nodes, `Chunk -> Entity` mention edges, semantic entity-to-entity relation edges, and conservative same-document chunk-to-chunk `RELATES_TO` links for shared high-confidence entities. Existing `AnchoredRescue` and `ArticleGraph` policies do not consume semantic entity seeds by default.
- The local eval database needed the new migration applied before rebuilding WixQA caches: `crates/moa-migrations/migrations/postgres/V000327__knowledge_semantic_graph_extractions.sql`.
- Rebuilt a 10-question / 100-article Gemini 2 semantic graph smoke cache: `wixqa-semgraph-m4-smoke-10q-100a`. The cache has 100 articles, 116 chunks, and 116 semantic extraction cache rows.
- ArticleGraph on the rebuilt semantic cache stayed safe: `.moa/wixqa/reports/simulated-10q-100a-tp-gemini2-1024-k25-article-graph-semantic-m4-final-smoke.json` reached recall/hit/MRR/NDCG 1.000, total p95 610 ms, and 0 hurt / 0 rescue / 10 neutral.
- Graph-off on the same cache also reached recall/hit/MRR/NDCG 1.000 with total p95 573 ms. This small set is too easy to show quality lift.
- A broad semantic entity seed attempt is explicitly rejected. Report `.moa/wixqa/reports/simulated-10q-100a-tp-gemini2-1024-k25-article-graph-semantic-m4-seeded.json` admitted 80 semantic seeds and 206 raw graph paths, then dropped MRR to 0.633 and NDCG to 0.719 with 5 hurt queries. This validates the plan's warning that semantic graph seeds must be exact, typed, and gated.
- Semantic entity query seeds are now reserved for explicit slow graph policies (`EntityLocalSearch`, `Propagation`, `Community`) and additionally require exact multi-token entity-name overlap. On the 10q smoke, the explicit `entity-local-search` report was neutral with recall/hit/MRR/NDCG 1.000 and p95 655 ms, but accepted 0 semantic seeds, so it is not yet a measured quality improvement.

## Next Experiment Queue

### P0: Reuse Existing Embeddings And Cache

These should run before any re-chunk/reembed work.

1. Article-level score aggregation. Completed first replay on 2026-07-05.
   - Compare article ranking by max chunk score, top-N saturated sum, and multi-hit agreement.
   - Success bar: improve MRR/NDCG over vector-first k25 without materially increasing retrieval p95.
2. Selective rerank. Replay completed on 2026-07-05; live harness experiment completed on 2026-07-06.
   - Trigger rerank only for uncertain queries: low top score, small rank gap, no repeated article in top chunks, or weak title overlap.
   - Success bar: capture at least half of chunk-text rerank quality gain with much lower p95 and cost.
3. BM25 rescue gating.
   - Use BM25 only when vector confidence is low.
   - Add BM25 candidates only as rescue candidates, not as broad additive fusion.
   - Require field/title/entity evidence before BM25 can affect top ranks.
4. Miss bucketing. Completed first bucket pass on 2026-07-05.
   - Classify current misses into vector-miss, right-article-low-rank, right-article-absent, BM25-rescuable, and rerank-rescuable.
   - Success bar: every later experiment targets a measured miss bucket.
5. Metadata-only scoring. First replay completed on 2026-07-06.
   - Use existing title, URL slug, `article_type`, and heading metadata for deterministic boosts.
   - Success bar: improve rank quality without reembedding.
6. Conditional k expansion. First live run completed on 2026-07-05.
   - Rerun retrieval at k50 only when top-10 source articles have no repeats.
   - Success bar: rescue k25 misses without paid rerank and keep total p95 within realtime tolerance.

### P1: Requires Turbopuffer Reprojection But Not New Embeddings

1. Field-weighted BM25.
   - Project separate searchable fields for title, heading path, body, URL slug, and article type.
   - Test BM25 field weights such as `title > heading > body`.
2. Metadata filters and boosts.
   - Store structured metadata that lets retrieval prefer exact title/product matches while preserving vector ranking.

### P2: Requires Rechunking And Reembedding

1. Chunking sweep. Chunk300 completed on 2026-07-06; medium and heading-aware variants deferred until chunk300's rebuild cost is justified by a larger gain.
   - Baseline: target 700, max 1000, min 120.
   - Small: target 300, max 500, min 80.
   - Medium: target 500, max 750, min 100.
   - Heading-aware: split on headings, include heading path, and add bounded overlap.
2. Parent/child retrieval.
   - Embed smaller child chunks, rank by chunk, then aggregate and return parent article context.
3. Article summary parent chunks.
   - Add one summary/title/heading chunk per article alongside body chunks.

### P3: Query-Side Alternatives

1. No-LLM query normalization.
   - Extract title-like phrases, URL-ish tokens, product names, and exact terms.
2. Conditional k expansion.
   - Start with k25; retry k50 only when confidence is low.
3. Pseudo-relevance feedback.
   - Use top vector titles/headings to issue a second retrieval pass only on low-confidence queries.

## Append Template

Use this template for every new run:

```markdown
| YYYY-MM-DD | run-name | `report-path.json` | backend / mode | K | rerank | recall | hit | mrr | ndcg | embed-p95 | retrieval-p95 | total-p95 | est-usd | verdict |
```

Record any code/config changes immediately above the row if the run cannot be reproduced from report metadata alone.

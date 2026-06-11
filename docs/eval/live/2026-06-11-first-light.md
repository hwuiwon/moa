# Memory Live Lane First Light - 2026-06-11

This note records the first live-lane artifact contract and the first local
PR-natural live run. Live reports are not baselines and must not be checked into
`docs/eval/baselines/`; nightly runs upload the JSON reports and delta tables as
workflow artifacts.

## Runs

| Run | Corpus | Lane | Reranker | Budget |
|---|---|---|---|---|
| Hermetic twin | `memory-eval-pr-natural-1-2-3` | `pr` | `off` | `0 USD` |
| Live | `memory-eval-pr-natural-1-2-3` | `live` | `off` | `5 USD` |
| Live rerank | `memory-eval-pr-natural-1-2-3` | `live` | `on` | `5 USD` |
| Full live scale | full natural seeds `1,2,3` | `live` | `on` | `15 USD` |

## Delta Tables

The first local paid run completed the hermetic-vs-live comparison with reranker
off. The reranker A/B and full natural scale run are produced by the nightly
workflow artifacts: `hermetic-vs-live-delta.txt`, `rerank-delta.txt`,
`full-live.json`.

Local reranker-on smoke was attempted with a five-probe target-only corpus, but
the available Cohere trial key had reached its monthly API-call cap before the
retrieval probes ran. The nightly job should use a production-capable key for
the reranker A/B and full natural run.

| Metric | Hermetic | Live | Delta |
|---|---:|---:|---:|
| `recall_at_4` | 0.480 | 0.603 | +0.123 |
| `mrr` | 0.413 | 0.569 | +0.156 |
| `ndcg_at_4` | 0.406 | 0.523 | +0.117 |
| `per_leg_recall.vector` | 0.432 | 0.864 | +0.432 |
| `per_leg_recall.graph` | 0.500 | 0.593 | +0.093 |
| `entity_fragmentation` | 1.141 | 1.148 | +0.007 |
| `p95_retrieval_latency_ms` | 0 | 2234 | +2234 |
| `cross_user_leak_count` | 0 | 0 | 0 |
| `pii_unredacted_count` | 0 | 0 | 0 |

Paired comparison on `memory-eval-pr-natural-1-2-3`: `recall_at_4` shipped
with CI95 `[+0.046,+0.179]`, adjusted `p = 0.022`.

## Cost Lines

The live reports carry `cost.pricing_as_of`, estimated embed/chat/rerank counts,
`cost.est_usd`, and `cost.budget_usd`.

| Run | `pricing_as_of` | Embed tokens | Chat input | Chat output | Rerank calls | Est. USD | Budget | Aborted |
|---|---|---:|---:|---:|---:|---:|---:|---|
| Live reranker off | 2026-06-11 | 4626 | 2650 | 4435 | 0 | 0.0515 | 5.00 | false |

## Follow-ups

- Recalibrate the prompt-07 `0.80` entity-blocking threshold against real Cohere
  embedding geometry using live `entity_fragmentation` and graph-leg recall.
- Decide whether Cohere reranking earns its latency and spend from the paired
  `live` vs `live-rerank` delta table.

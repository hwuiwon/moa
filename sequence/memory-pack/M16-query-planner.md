# Step M16 — Query planner (NER + scope, retrieval strategy selection)

_Build the query planner that turns a free-form user query into a typed `RetrievalRequest`: NER seed nodes (via `node_index.name_tsv`), retrieval strategy choice (GraphFirst / VectorFirst / Both), scope expansion to ancestors, and label hints._

## 1 What this step is about

The retriever (M15) runs three legs in parallel and lets RRF sort them out. But for a graph-heavy query ("what depends on the auth service?") the graph leg is decisive; for a fuzzy semantic query ("how does authentication usually fail?") the vector leg dominates. The planner classifies the query, finds NER seeds in the sidecar, expands `MemoryScope::ancestors()` for the retriever's scope filter, and decides whether to skip a leg entirely. This is a small, fast model (~5ms) — not an LLM call.

## 2 Files to read

- M01 `MemoryScope::ancestors`
- M04 `node_index.name_tsv` and `lookup_seeds`
- M15 `RetrievalRequest`
- M07 `GraphStore::lookup_seeds`

## 3 Goal

```rust
pub struct QueryPlanner { /* ... */ }

#[derive(Debug, Clone)]
pub struct PlannedQuery {
    pub strategy: Strategy,                    // {GraphFirst, VectorFirst, Both}
    pub seeds: Vec<Uuid>,
    pub label_hint: Option<Vec<NodeLabel>>,
    pub scope: MemoryScope,
    pub temporal_filter: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy)]
pub enum Strategy { GraphFirst, VectorFirst, Both }

impl QueryPlanner {
    pub async fn plan(&self, query_text: &str, ctx: &PlanningCtx) -> Result<PlannedQuery>;
}
```

## 4 Rules

- **Lightweight NER**: a small bundled model (e.g., a 100MB ONNX BERT) or a regex+gazetteer fallback for v1. No LLM call here — too slow.
- **Strategy heuristics** are explicit in code (not a learned classifier in v1):
  - "What depends on / connects to / impacts X" → GraphFirst.
  - "When did / how often / has anything been done about" → VectorFirst.
  - Default → Both.
- **Seed lookup uses `lookup_seeds`** (M04 helper), top-5 per detected entity name.
- **Scope** comes from request context (`ScopeContext::scope`); planner expands it via `ancestors()` for the retriever to fan out across all tiers.

## 5 Tasks

### 5a NER

`crates/moa-brain/src/planning/ner.rs`:

```rust
pub struct NerExtractor { /* ONNX session OR regex+gazetteer */ }

impl NerExtractor {
    pub fn extract(&self, text: &str) -> Vec<NerSpan> { /* ~5ms */ }
}

pub struct NerSpan { pub start: usize, pub end: usize, pub text: String, pub label: NerLabel }
pub enum NerLabel { Person, Org, Product, Concept, Place, Other }
```

For v1 the implementation can be a hybrid:
- Regex for emails, URLs, file paths, common code identifiers.
- A small ONNX BERT-base-NER for natural language.
- Gazetteer of workspace-known entity names from `moa.node_index` (cached, refreshed every 60s).

### 5b Strategy classifier

```rust
fn classify_strategy(text: &str) -> Strategy {
    let lower = text.to_ascii_lowercase();
    if lower.contains("depends on") || lower.contains("connects to") || lower.contains("impacted by")
        || lower.contains("relate") || lower.contains("upstream") || lower.contains("downstream") {
        return Strategy::GraphFirst;
    }
    if lower.contains("when ") || lower.contains("how often") || lower.contains("history of")
        || lower.contains("similar to") {
        return Strategy::VectorFirst;
    }
    Strategy::Both
}
```

### 5c Plan assembly

```rust
pub async fn plan(&self, query_text: &str, ctx: &PlanningCtx) -> Result<PlannedQuery> {
    let spans = self.ner.extract(query_text);
    let mut seeds = vec![];
    for span in &spans {
        let candidates = self.graph.lookup_seeds(&span.text, 3).await?;
        seeds.extend(candidates.into_iter().map(|n| n.uid));
    }
    seeds.sort(); seeds.dedup();

    let strategy = classify_strategy(query_text);
    let label_hint = infer_label_hint(query_text, &spans);
    let temporal = parse_temporal(query_text);
    Ok(PlannedQuery {
        strategy, seeds, label_hint,
        scope: ctx.scope.clone(),
        temporal_filter: temporal,
    })
}

fn infer_label_hint(text: &str, _: &[NerSpan]) -> Option<Vec<NodeLabel>> {
    let l = text.to_ascii_lowercase();
    if l.contains("decision") || l.contains("decided") { return Some(vec![NodeLabel::Decision]); }
    if l.contains("incident") || l.contains("outage")  { return Some(vec![NodeLabel::Incident]); }
    if l.contains("lesson") || l.contains("learned")   { return Some(vec![NodeLabel::Lesson]); }
    None
}

fn parse_temporal(text: &str) -> Option<DateTime<Utc>> {
    // crude: "as of 2025-01-01", "before yesterday", etc. via dateparser crate
    None  // v1 stub; full impl in v1.1
}
```

### 5d Wire into retrieval pipeline

`crates/moa-brain/src/pipeline/memory_retriever.rs`:

```rust
pub async fn retrieve_for_query(query: &str, scope: MemoryScope, ctx: &RetrievalCtx) -> Result<Vec<RetrievalHit>> {
    let planned = ctx.planner.plan(query, &PlanningCtx { scope, /*...*/ }).await?;
    let embedding = ctx.embedder.embed(&[query.to_string()]).await?.remove(0);

    // Use strategy to skip legs in the hybrid retriever (extension to M15).
    let req = RetrievalRequest {
        seeds: planned.seeds,
        query_text: query.to_string(),
        query_embedding: embedding,
        scope: planned.scope,
        label_filter: planned.label_hint,
        max_pii_class: ctx.actor_pii_clearance,
        k_final: 5,
        use_reranker: ctx.use_reranker,
    };
    ctx.hybrid.retrieve(req).await
}
```

### 5e Strategy override on the retriever

Update `HybridRetriever::retrieve` to accept an optional `Strategy` and skip legs accordingly:

```rust
match strategy {
    Strategy::GraphFirst  => { /* run all 3 but lexical weight halved */ }
    Strategy::VectorFirst => { /* skip graph leg if seeds empty */ }
    Strategy::Both        => { /* default fan-out */ }
}
```

## 6 Deliverables

- `crates/moa-brain/src/planning/{mod,ner,planner}.rs` (~400 lines).
- ONNX model file or gazetteer in `crates/moa-brain/assets/`.
- Updated `pipeline/memory_retriever.rs`.

## 7 Acceptance criteria

1. "What depends on the auth service?" plans GraphFirst with at least one auth-related seed.
2. "How often does the deploy fail?" plans VectorFirst.
3. Planner latency P95 <8ms.
4. Seeds returned by NER all exist in `moa.node_index` (i.e., gazetteer-grounded).

## 8 Tests

```sh
cargo test -p moa-brain planner_classify
cargo test -p moa-brain ner_smoke
cargo bench -p moa-brain plan_p95
```

## 9 Cleanup

- Remove any old "search wiki by keyword" path that existed pre-graph.
- If a previous embed-only retrieval helper exists in `moa-brain`, delete it.

## 10 What's next

**M17 — Read-time cache (changelog-version invalidation).**

# Step M15 — Hybrid retriever (graph + vector + lexical, RRF k=60, Cohere Rerank v4.0-fast)

_Build the production hybrid retriever in `moa-brain` that fuses three retrieval legs (graph traversal, vector KNN, Postgres tsvector) via RRF k=60 and optionally reranks the top candidates with Cohere Rerank v4.0-fast._

## 1 What this step is about

Three retrieval legs in parallel, fused by RRF:

- **Graph leg**: 1-3 hop traversal from NER seed nodes (planner from M16 supplies seeds).
- **Vector leg**: KNN k=20 against `moa.embeddings`.
- **Lexical leg**: Postgres tsvector match on `name_tsv` and any future tsvector columns.

Default fusion weights `w_g=1.2, w_v=1.0, w_l=0.8`. Rerank top-25 fused candidates to top-5 via Cohere Rerank v4.0-fast. Reranking is feature-flagged per workspace (`workspace_state.use_reranker`, default true for HIPAA workspaces, false for cost-sensitive).

## 2 Files to read

- M00 stack-pin (Cohere Rerank v4.0-fast)
- M07 GraphStore::neighbors
- M05 VectorStore::knn
- M04 node_index.name_tsv
- M16 query planner (next prompt; this prompt accepts the planner's output as input)

## 3 Goal

`crates/moa-brain/src/retrieval/hybrid.rs` exposing:

```rust
pub struct HybridRetriever { /* ... */ }

#[derive(Debug, Clone)]
pub struct RetrievalRequest {
    pub seeds: Vec<Uuid>,                  // from query planner
    pub query_text: String,
    pub query_embedding: Vec<f32>,
    pub scope: MemoryScope,
    pub label_filter: Option<Vec<NodeLabel>>,
    pub max_pii_class: PiiClass,
    pub k_final: usize,                    // typical 5-10
    pub use_reranker: bool,
}

pub struct RetrievalHit {
    pub uid: Uuid,
    pub score: f64,
    pub legs: LegSources,                  // which legs contributed
    pub node: NodeIndexRow,
}

impl HybridRetriever {
    pub async fn retrieve(&self, req: RetrievalRequest) -> Result<Vec<RetrievalHit>>;
}
```

## 4 Rules

- **Three legs run concurrently** via `tokio::join!`.
- **RRF k=60** (canonical).
- **Per-leg latency budget**: graph 30ms, vector 25ms, lexical 15ms; total leg-fan-out budget 50ms (each tracked independently). Reranker adds ~50ms; total budget ~120ms warm.
- **Layer-priority bias** (post-RRF): apply `User × 1.3`, `Workspace × 1.1`, `Global × 1.0` multipliers based on `node.scope`.
- **Cache-aware**: read-time cache (M17) wraps this whole call; the retriever itself has no cache.

## 5 Tasks

### 5a Three legs

```rust
async fn graph_leg(&self, req: &RetrievalRequest) -> Result<Vec<(Uuid, f64)>> {
    // Walk neighbors of each seed; merge dedup; assign rank by hop distance.
    let mut all = vec![];
    for seed in &req.seeds {
        for hops in 1..=2u8 {
            let nodes = self.graph.neighbors(*seed, hops, None).await?;
            for (i, n) in nodes.iter().enumerate() { all.push((n.uid, 1.0 / (60.0 + i as f64))); }
        }
    }
    Ok(merge_dedup(all))
}

async fn vector_leg(&self, req: &RetrievalRequest) -> Result<Vec<(Uuid, f64)>> {
    let q = VectorQuery {
        embedding: req.query_embedding.clone(),
        k: 20,
        label_filter: req.label_filter.as_ref().map(|v| v.iter().map(|l| l.as_str().into()).collect()),
        max_pii_class: req.max_pii_class.as_str().into(),
        include_global: true,
    };
    let hits = self.vector.knn(&q).await?;
    Ok(hits.into_iter().enumerate().map(|(rank, h)| (h.uid, 1.0 / (60.0 + rank as f64 + 1.0))).collect())
}

async fn lexical_leg(&self, req: &RetrievalRequest) -> Result<Vec<(Uuid, f64)>> {
    let rows: Vec<(Uuid, f32)> = sqlx::query_as(
        "SELECT uid, ts_rank(name_tsv, plainto_tsquery('simple', $1)) AS rank
         FROM moa.node_index
         WHERE valid_to IS NULL AND name_tsv @@ plainto_tsquery('simple', $1)
         ORDER BY rank DESC LIMIT 20"
    ).bind(&req.query_text).fetch_all(&self.pool).await?;
    Ok(rows.into_iter().enumerate()
        .map(|(rank, (uid, _))| (uid, 1.0 / (60.0 + rank as f64 + 1.0))).collect())
}
```

### 5b Fusion

```rust
fn rrf_fuse(g: Vec<(Uuid,f64)>, v: Vec<(Uuid,f64)>, l: Vec<(Uuid,f64)>,
            (wg,wv,wl): (f64,f64,f64)) -> Vec<(Uuid, f64, LegSources)> {
    let mut scores: HashMap<Uuid, (f64, LegSources)> = HashMap::new();
    for (u, s) in g { let e = scores.entry(u).or_insert((0.0, LegSources::default())); e.0 += s * wg; e.1.graph = true; }
    for (u, s) in v { let e = scores.entry(u).or_insert((0.0, LegSources::default())); e.0 += s * wv; e.1.vector = true; }
    for (u, s) in l { let e = scores.entry(u).or_insert((0.0, LegSources::default())); e.0 += s * wl; e.1.lexical = true; }
    let mut out: Vec<_> = scores.into_iter().map(|(u, (s, src))| (u, s, src)).collect();
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    out
}
```

### 5c Layer-priority bias

```rust
fn apply_layer_bias(rows: &[NodeIndexRow], base: &mut [(Uuid, f64, LegSources)]) {
    let mult: HashMap<Uuid, f64> = rows.iter().map(|n| (n.uid, match n.scope.as_str() {
        "user" => 1.3, "workspace" => 1.1, _ => 1.0,
    })).collect();
    for (uid, score, _) in base.iter_mut() {
        if let Some(m) = mult.get(uid) { *score *= m; }
    }
    base.sort_by(|a,b| b.1.partial_cmp(&a.1).unwrap());
}
```

### 5d Top-level retrieve

```rust
pub async fn retrieve(&self, req: RetrievalRequest) -> Result<Vec<RetrievalHit>> {
    let (g, v, l) = tokio::try_join!(self.graph_leg(&req), self.vector_leg(&req), self.lexical_leg(&req))?;
    let mut fused = rrf_fuse(g, v, l, (1.2, 1.0, 0.8));
    fused.truncate(25);

    // Hydrate
    let uids: Vec<Uuid> = fused.iter().map(|(u,_,_)| *u).collect();
    let nodes = sqlx::query_as!(NodeIndexRow, "...WHERE uid = ANY($1) AND valid_to IS NULL", &uids)
        .fetch_all(&self.pool).await?;

    apply_layer_bias(&nodes, &mut fused);

    // Optional rerank top 25 → req.k_final
    if req.use_reranker && fused.len() > req.k_final {
        let docs: Vec<String> = uids.iter().filter_map(|u| nodes.iter().find(|n| &n.uid == u).map(|n| n.name.clone())).collect();
        let rr = self.reranker.rerank("rerank-v4.0-fast", &req.query_text, &docs, req.k_final).await?;
        return Ok(rr.into_iter().map(|hit| {
            let (uid, score, legs) = fused[hit.index].clone();
            let node = nodes.iter().find(|n| n.uid == uid).unwrap().clone();
            RetrievalHit { uid, score, legs, node }
        }).collect());
    }

    Ok(fused.into_iter().take(req.k_final).map(|(u,s,l)| {
        let node = nodes.iter().find(|n| n.uid == u).unwrap().clone();
        RetrievalHit { uid: u, score: s, legs: l, node }
    }).collect())
}
```

### 5e Bump last_accessed_at

After successful retrieval, fire-and-forget:

```rust
tokio::spawn(async move {
    let _ = sqlx::query!("UPDATE moa.node_index SET last_accessed_at = now() WHERE uid = ANY($1)", &uids).execute(&pool).await;
});
```

## 6 Deliverables

- `crates/moa-brain/src/retrieval/hybrid.rs` (~500 lines).
- `crates/moa-brain/src/retrieval/legs.rs`.
- Cohere reranker thin client in `moa-brain` (or shared crate).
- Bench fixtures.

## 7 Acceptance criteria

1. End-to-end retrieval against a 1000-fact workspace returns 5 results with correct legs annotations.
2. Layer bias: a User-scoped exact match outranks a Workspace-scoped exact match for the same query.
3. Reranker on/off swap works without code changes.
4. P95 latency <80ms warm without rerank, <120ms with rerank (per M30 target).

## 8 Tests

```sh
cargo test -p moa-brain hybrid_retrieval_e2e
cargo bench -p moa-brain hybrid_p95
```

## 9 Cleanup

- Remove old single-leg retriever from `moa-brain/src/pipeline/memory_retriever.rs`.
- Remove old "wiki page rank" logic.
- Delete `MemoryRetriever` enum if it had `Wiki | Vector` variants — only `Hybrid` exists now.

## 10 What's next

**M16 — Query planner (NER, scope, layer-bias seeds for the retriever).**

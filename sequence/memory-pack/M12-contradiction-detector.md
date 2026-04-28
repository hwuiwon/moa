# Step M12 — Contradiction detector (vector + lexical → RRF → LLM judge)

_Build the typed `ContradictionDetector` trait and `RrfPlusJudgeDetector` impl that the slow path (M10) and fast path (M11) both call. Returns one of `Conflict::{Insert, Supersede(uid), Duplicate(uid), Indeterminate}` per fact._

## 1 What this step is about

Naive nearest-neighbor matching produces too many false-positive supersessions ("we deploy to fly.io" vs "we deploy to AWS" both score high cosine similarity but mean different things). The detector retrieves K=10 candidate facts via hybrid retrieval (vector + lexical) scoped to the **same entity pair** (e.g., subject="deploy", object="cloud-provider"), reranks to top-N=5 via Cohere Rerank v4.0-fast, then asks a fast LLM judge "is the new fact CONTRADICTING, RESTATING, or INDEPENDENT of each candidate?" The judge response routes to the four-way Conflict enum.

## 2 Files to read

- M00 stack-pin (Cohere Rerank v4.0-fast)
- M07 GraphStore (for candidate reads)
- M05 VectorStore (for KNN)
- M11 (fast-path latency budget)

## 3 Goal

`ContradictionDetector` trait + `RrfPlusJudgeDetector` impl with K=10, top-N=5, RRF k=60, and a 250ms total budget for fast-path or 5s for slow-path.

## 4 Rules

- **Same-entity-pair scoping**: candidates must share at least one entity uid with the new fact's extracted entity pair, or share a label.
- **RRF k=60** for fusing vector and lexical ranks.
- **Cohere Rerank v4.0-fast** for top-N=5 selection.
- **LLM judge prompt is fixed** and committed in `crates/moa-memory-ingest/prompts/judge.txt`; cached on `(fact_hash, candidate_uid)`.
- **Conflict enum is exhaustive**:

```rust
pub enum Conflict {
    Insert,                    // no contradicting fact found
    Supersede(Uuid),           // strict contradiction; supersede that uid
    Duplicate(Uuid),           // restatement; return existing uid
    Indeterminate,             // judge abstained or budget exceeded
}
```

## 5 Tasks

### 5a Trait

`crates/moa-memory-ingest/src/contradiction.rs`:

```rust
#[async_trait::async_trait]
pub trait ContradictionDetector: Send + Sync {
    async fn check_one_fast(&self, fact_text: &str, embedding: &[f32], label: NodeLabel, ctx: &Ctx)
        -> Result<Conflict, IngestError>;
    async fn check_one_slow(&self, fact: &EmbeddedFact, ctx: &Ctx)
        -> Result<Conflict, IngestError>;
}
```

### 5b RRF candidate retrieval

```rust
async fn candidates(&self, fact_text: &str, embedding: &[f32], label: NodeLabel, ctx: &Ctx)
    -> Result<Vec<NodeIndexRow>, IngestError>
{
    // 1. Vector KNN k=10, label-filtered
    let vec_hits = ctx.vector.knn(&VectorQuery {
        embedding: embedding.to_vec(), k: 10,
        label_filter: Some(vec![label.as_str().into()]),
        max_pii_class: "phi".into(), include_global: true,
    }).await?;

    // 2. Lexical via tsvector on name
    let lex_hits: Vec<Uuid> = sqlx::query_scalar!(
        "SELECT uid FROM moa.node_index
         WHERE valid_to IS NULL AND name_tsv @@ plainto_tsquery('simple', $1)
         ORDER BY ts_rank(name_tsv, plainto_tsquery('simple', $1)) DESC LIMIT 10",
        fact_text
    ).fetch_all(&ctx.pool).await?;

    // 3. RRF fuse (k=60)
    let mut scores: HashMap<Uuid, f64> = HashMap::new();
    for (rank, hit) in vec_hits.iter().enumerate() {
        *scores.entry(hit.uid).or_default() += 1.0 / (60.0 + rank as f64 + 1.0);
    }
    for (rank, uid) in lex_hits.iter().enumerate() {
        *scores.entry(*uid).or_default() += 1.0 / (60.0 + rank as f64 + 1.0);
    }
    let mut ranked: Vec<(Uuid, f64)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    let uids: Vec<Uuid> = ranked.into_iter().take(10).map(|(u,_)| u).collect();
    if uids.is_empty() { return Ok(vec![]); }

    // 4. Hydrate
    sqlx::query_as!(NodeIndexRow,
        "SELECT uid, label as \"label: _\", workspace_id, user_id, scope, name,
                pii_class as \"pii_class: _\", valid_to, last_accessed_at
         FROM moa.node_index WHERE uid = ANY($1) AND valid_to IS NULL",
        &uids,
    ).fetch_all(&ctx.pool).await.map_err(Into::into)
}
```

### 5c Cohere Rerank top-N=5

```rust
async fn rerank_top5(&self, fact_text: &str, candidates: &[NodeIndexRow])
    -> Result<Vec<NodeIndexRow>, IngestError>
{
    let docs: Vec<String> = candidates.iter().map(|c| c.name.clone()).collect();
    let resp = self.cohere_rerank.rerank(
        "rerank-v4.0-fast", fact_text, &docs, /*top_n*/ 5
    ).await?;
    Ok(resp.into_iter().map(|hit| candidates[hit.index].clone()).collect())
}
```

### 5d LLM judge

```rust
async fn judge(&self, fact_text: &str, candidates: &[NodeIndexRow])
    -> Result<Conflict, IngestError>
{
    if candidates.is_empty() { return Ok(Conflict::Insert); }
    let prompt = build_judge_prompt(fact_text, candidates);
    let cache_key = blake3::hash(prompt.as_bytes());
    if let Some(c) = self.judge_cache.get(&cache_key) { return Ok(c); }

    let resp: JudgeResponse = self.judge_llm.complete_structured(
        &prompt, JudgeResponse::schema(), Duration::from_millis(200)
    ).await?;

    let conflict = match resp.verdict.as_str() {
        "CONTRADICTS"  => Conflict::Supersede(resp.candidate_uid),
        "RESTATES"     => Conflict::Duplicate(resp.candidate_uid),
        "INDEPENDENT"  => Conflict::Insert,
        _              => Conflict::Indeterminate,
    };
    self.judge_cache.insert(cache_key, conflict.clone());
    Ok(conflict)
}
```

### 5e Judge prompt template

`crates/moa-memory-ingest/prompts/judge.txt`:

```
You are a fact-comparison judge. Given a NEW fact and CANDIDATE facts already
recorded, label the new fact's relationship to the SINGLE most-related candidate:

- CONTRADICTS  : the new fact, if true, makes the candidate false.
- RESTATES     : the new fact says the same thing as the candidate.
- INDEPENDENT  : the facts are unrelated or compatible.

Output JSON: {"verdict": "...", "candidate_uid": "uuid", "rationale": "..."}.

NEW FACT:
{{ fact_text }}

CANDIDATES (uid → name):
{{ candidates_list }}
```

## 6 Deliverables

- `crates/moa-memory-ingest/src/contradiction.rs` (~400 lines).
- `crates/moa-memory-ingest/prompts/judge.txt`.
- Cache impl: `moka` LRU keyed on prompt hash, 10k entries.

## 7 Acceptance criteria

1. New fact restating an existing one → `Duplicate(uid)`.
2. New fact contradicting an existing one (e.g., "deploy to AWS" after "deploy to fly.io") → `Supersede(uid)`.
3. Empty candidate list → `Insert`.
4. Judge times out at 200ms → `Indeterminate`.
5. Cache hit on identical prompt — second call <5ms.

## 8 Tests

```sh
cargo test -p moa-memory-ingest contradiction_judge
cargo test -p moa-memory-ingest rrf_fusion
```

## 9 Cleanup

- Remove any old wiki "diff and merge" code.
- Remove any code that detected page-level conflicts in MEMORY.md.

## 10 What's next

**M13 — Split vector code out of the legacy `moa-memory` crate.**

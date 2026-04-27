# Step M11 — Fast-path ingestion (remember / forget / supersede with <500ms target)

_Synchronous ingestion path for explicit user/agent commands ("remember X", "forget Y", "X supersedes Z"). Latency budget is <500ms p95; falls back to a rule-based contradiction check if the LLM judge times out at 250ms._

## 1 What this step is about

The slow path (M10) is right for passive learning. But when a user types "remember that we deploy to fly.io" or an agent flags "this contradicts the last time," we need it written before the response returns. The fast path runs the same `GraphStore::create_node` / `supersede_node` / `invalidate_node` protocol but with bounded latency, a single LLM judge call (Haiku-class, fast), and rule-based fallback when the judge can't answer in time.

## 2 Files to read

- M07/M08 GraphStore + write protocol
- M10 slow-path step shapes (we reuse `extract_facts`/`classify_facts`/`embed_batch`)
- M09 PiiClassifier
- M12 contradiction detector trait (full impl in M12; stubbed here)

## 3 Goal

Public API:

```rust
pub async fn fast_remember(req: FastRememberRequest, ctx: &Ctx) -> Result<Uuid, FastError>;
pub async fn fast_forget(pattern: ForgetPattern, ctx: &Ctx) -> Result<u64, FastError>;
pub async fn fast_supersede(old_uid: Uuid, new: NodeWriteIntent, ctx: &Ctx) -> Result<Uuid, FastError>;
```

Each returns within 500ms p95 on the local-mode stack. The contradiction-check budget is 250ms; if exceeded, the Indeterminate path is taken (commit-with-warning per hwuiwon-DECISION Q2 from prior design).

## 4 Rules

- **Single embedder call** (small batch, k=1).
- **Single LLM judge call** with K=3 candidate facts, `rerank-v4.0-fast` for top-3 selection.
- **Hard timeout 250ms** on the LLM judge (use `tokio::time::timeout`). On timeout: rule-based fallback (string match on supersedes_specific; pattern match on forget).
- **Same atomic write protocol** as the slow path (M08).
- **`forget`** never decrypts; it matches on `name`, `properties_summary`, or explicit `uid`.

## 5 Tasks

### 5a `FastRememberRequest`

```rust
#[derive(Debug, Clone)]
pub struct FastRememberRequest {
    pub workspace_id: Uuid,
    pub user_id: Option<Uuid>,
    pub scope: String,
    pub text: String,                       // free-form fact
    pub label: NodeLabel,                   // typically Fact, sometimes Decision/Lesson
    pub supersedes_specific: Option<Uuid>,  // explicit supersession
    pub actor_id: Uuid,
    pub actor_kind: String,
}
```

### 5b `fast_remember`

```rust
pub async fn fast_remember(req: FastRememberRequest, ctx: &Ctx) -> Result<Uuid, FastError> {
    // 1. Validate
    if req.text.is_empty() { return Err(FastError::Invalid("empty text".into())); }

    // 2. Embed (~80ms)
    let emb = ctx.embedder.embed(&[req.text.clone()]).await?
        .into_iter().next().ok_or(FastError::Embed("no result".into()))?;

    // 3. PII classify (~30ms; can run in parallel with embed)
    let pii = ctx.pii.classify(&req.text).await.unwrap_or(PiiResult {
        class: PiiClass::Pii, spans: vec![], model_version: "fallback".into(), abstained: true,
    });

    // 4. Contradiction check with hard 250ms timeout
    let conflict = if let Some(old) = req.supersedes_specific {
        Conflict::Supersede(old)
    } else {
        match tokio::time::timeout(Duration::from_millis(250),
            ctx.contradict.check_one_fast(&req.text, &emb, req.label, ctx)).await {
            Ok(Ok(c)) => c,
            Ok(Err(_)) | Err(_) => Conflict::Indeterminate, // fall through to commit
        }
    };

    // 5. Build intent
    let intent = NodeWriteIntent {
        uid: Uuid::now_v7(),
        label: req.label,
        workspace_id: Some(req.workspace_id), user_id: req.user_id,
        scope: req.scope.clone(), name: short_name(&req.text),
        properties: serde_json::json!({"summary": req.text}),
        pii_class: pii.class,
        confidence: Some(0.9),
        valid_from: Utc::now(),
        embedding: Some(emb),
        embedding_model: Some(ctx.embedder.model_name().into()),
        embedding_model_version: Some(ctx.embedder.model_version()),
        actor_id: req.actor_id, actor_kind: req.actor_kind.clone(),
    };

    // 6. Write atomically
    match conflict {
        Conflict::Supersede(old_uid)        => ctx.graph.supersede_node(old_uid, intent).await.map_err(Into::into),
        Conflict::Insert | Conflict::Indeterminate => ctx.graph.create_node(intent).await.map_err(Into::into),
        Conflict::Duplicate(existing_uid)   => Ok(existing_uid),
    }
}
```

### 5c `fast_forget`

```rust
#[derive(Debug, Clone)]
pub enum ForgetPattern {
    Uid(Uuid),
    NameMatch(String),    // exact name match
    SoftAll(Uuid),        // forget everything by a specific user_id within current workspace
}

pub async fn fast_forget(pattern: ForgetPattern, ctx: &Ctx) -> Result<u64, FastError> {
    // Soft invalidation by default. Hard purge only via M24 CLI.
    let uids: Vec<Uuid> = match pattern {
        ForgetPattern::Uid(u) => vec![u],
        ForgetPattern::NameMatch(n) => sqlx::query_scalar!(
            "SELECT uid FROM moa.node_index WHERE name = $1 AND valid_to IS NULL", n
        ).fetch_all(&ctx.pool).await?,
        ForgetPattern::SoftAll(uid) => sqlx::query_scalar!(
            "SELECT uid FROM moa.node_index WHERE user_id = $1 AND valid_to IS NULL", uid
        ).fetch_all(&ctx.pool).await?,
    };
    let mut count = 0;
    for u in uids {
        ctx.graph.invalidate_node(u, "user_forget").await?;
        count += 1;
    }
    Ok(count)
}
```

### 5d `fast_supersede`

Already covered by `GraphStore::supersede_node`. Wrapper just enforces the latency budget around it.

### 5e Wire into the brain

In `moa-brain` tool registry, register three new tools:

- `memory.remember(text, label="Fact")` → `fast_remember`
- `memory.forget(pattern)` → `fast_forget`
- `memory.supersede(old_uid, new_text)` → `fast_supersede`

Tool definitions go through the tool registry; agent invokes them like any other tool call.

### 5f Metrics

```rust
metrics::histogram!("moa_fast_remember_latency_seconds", elapsed.as_secs_f64());
metrics::counter!("moa_fast_remember_total", "outcome" => outcome).increment(1);
metrics::counter!("moa_fast_remember_indeterminate_total").increment(if indeterminate { 1 } else { 0 });
```

## 6 Deliverables

- `crates/moa-orchestrator/src/fast_path.rs` (~250 lines).
- `crates/moa-brain/src/tools/memory_tools.rs` registering the three tools.
- Latency-budget integration test.

## 7 Acceptance criteria

1. `fast_remember("we deploy to fly.io")` returns within 500ms p95 (10-run benchmark).
2. With `supersedes_specific=Some(old)`, the old node is invalidated and a SUPERSEDES edge created.
3. LLM-judge timeout falls through to `Conflict::Indeterminate` and commits with `confidence=0.5`.
4. `fast_forget(NameMatch("auth"))` invalidates all matching nodes; second call invalidates 0.
5. Tools appear in `moa-brain` tool registry.

## 8 Tests

```sh
cargo test -p moa-orchestrator fast_remember_e2e
cargo test -p moa-orchestrator fast_forget_idempotent
cargo bench -p moa-orchestrator fast_remember_p95
```

## 9 Cleanup

- Remove any old "remember" tool that wrote to filesystem MEMORY.md. The new tool path is the only one.
- Old "compact" / "summarize wiki" tool no longer exists; delete from registry.

## 10 What's next

**M12 — Contradiction detector (vector + lexical → RRF → LLM judge).**

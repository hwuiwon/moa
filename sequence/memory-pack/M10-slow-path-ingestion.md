# Step M10 — Slow-path ingestion VO in Restate (chunk → extract → contradict → upsert)

_Build the asynchronous, durable, idempotent ingestion pipeline that turns finalized session turns into graph nodes and edges within a 60-second SLA. Restate Virtual Objects keyed on `(workspace_id, session_id)` give us crash-safe, replayable, sequential processing per session._

## 1 What this step is about

The slow path is best-effort consistency, not durability. Sessions are durably stored in `moa.session_turns` regardless of whether ingestion succeeds. The ingestion VO chunks the turn transcript, runs an LLM extractor for entities/facts, calls the contradiction detector (M12) for each fact, and finally writes through `GraphStore` (M07/M08) inside the atomic write protocol. Idempotency is enforced at `(workspace_id, session_id, turn_seq, fact_hash)`.

## 2 Files to read

- M00 stack-pin (Restate 1.6.1+)
- M07/M08 GraphStore + write protocol
- M09 PiiClassifier
- M05 Embedder + VectorStore
- M12 contradiction detector (will be impl'd next; stub trait here)

## 3 Goal

1. New crate scaffold updates: `moa-memory-ingest` Cargo.toml, lib stubs (full crate scaffold land in M14; for now the IngestionVO lives in `moa-orchestrator`).
2. `IngestionVO` Virtual Object with `ingest_turn(turn)` handler.
3. Restate journal steps: chunk → extract → classify_pii → embed → contradict → upsert → mark_done.
4. Idempotency via `moa.ingest_dedup` table.
5. Backpressure flag on workspace (`slow_path_degraded`) when queue depth exceeds threshold.

## 4 Rules

- **Each Restate step is `ctx.run("name", || async { ... })`** so the journal replays deterministically on retry.
- **Each step is independently retryable** with bounded exponential backoff.
- **Idempotency table** `moa.ingest_dedup` UNIQUE on `(workspace_id, session_id, turn_seq, fact_hash)`.
- **Concurrency**: VOs keyed on `(workspace_id, session_id)` are serialized by Restate; cross-session parallelism is bounded by per-workspace concurrency cap (`workspace_state.ingest_concurrency`, default 8).
- **Failure isolation**: a failing fact does not block its turn; failed facts go to `moa.ingest_dlq` and are retried by a separate VO.

## 5 Tasks

### 5a Migration

`migrations/M10_ingest.sql`:

```sql
CREATE TABLE moa.ingest_dedup (
    workspace_id UUID NOT NULL,
    session_id   UUID NOT NULL,
    turn_seq     BIGINT NOT NULL,
    fact_hash    BYTEA NOT NULL,
    fact_uid     UUID NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, session_id, turn_seq, fact_hash)
);

CREATE TABLE moa.ingest_dlq (
    dlq_id BIGSERIAL PRIMARY KEY,
    workspace_id UUID NOT NULL,
    session_id   UUID,
    turn_seq     BIGINT,
    payload      JSONB NOT NULL,
    error        TEXT NOT NULL,
    retry_count  INT NOT NULL DEFAULT 0,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    next_retry_at TIMESTAMPTZ
);

ALTER TABLE moa.ingest_dedup ENABLE ROW LEVEL SECURITY; ALTER TABLE moa.ingest_dedup FORCE ROW LEVEL SECURITY;
ALTER TABLE moa.ingest_dlq   ENABLE ROW LEVEL SECURITY; ALTER TABLE moa.ingest_dlq   FORCE ROW LEVEL SECURITY;

CREATE POLICY ws ON moa.ingest_dedup FOR ALL TO moa_app
  USING (workspace_id = moa.current_workspace())
  WITH CHECK (workspace_id = moa.current_workspace());
CREATE POLICY ws ON moa.ingest_dlq   FOR ALL TO moa_app
  USING (workspace_id = moa.current_workspace())
  WITH CHECK (workspace_id = moa.current_workspace());

GRANT SELECT, INSERT, UPDATE, DELETE ON moa.ingest_dedup TO moa_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON moa.ingest_dlq   TO moa_app;

ALTER TABLE moa.workspace_state ADD COLUMN IF NOT EXISTS slow_path_degraded BOOLEAN NOT NULL DEFAULT false;
ALTER TABLE moa.workspace_state ADD COLUMN IF NOT EXISTS ingest_concurrency INT NOT NULL DEFAULT 8;
```

### 5b VO sketch

`crates/moa-orchestrator/src/ingestion_vo.rs`:

```rust
use restate_sdk::prelude::*;

#[restate_sdk::object]
impl IngestionVO {
    /// Ingest a finalized session turn. Idempotent on (session_id, turn_seq, fact_hash).
    pub async fn ingest_turn(ctx: ObjectContext<'_>, turn: SessionTurn) -> HandlerResult<()> {
        let key = format!("done:{}", turn.turn_seq);
        if ctx.get::<bool>(&key).await?.unwrap_or(false) { return Ok(()); }

        // 1. Chunk
        let chunks = ctx.run("chunk", || async { chunk_turn(&turn) }).await?;

        // 2. LLM extraction (entities, facts, candidate edges)
        let extracted = ctx.run("extract", || async { extract_facts(&chunks).await }).await?;

        // 3. PII classification (per fact text)
        let classified = ctx.run("pii", || async {
            classify_facts(&extracted).await
        }).await?;

        // 4. Embedding (batch)
        let embedded = ctx.run("embed", || async {
            embed_batch(&classified).await
        }).await?;

        // 5. Contradiction detection (per fact, scoped to same-entity-pair, M12)
        let decisions = ctx.run("contradict", || async {
            detect_contradictions(&embedded).await
        }).await?;

        // 6. Atomic upsert via GraphStore
        ctx.run("upsert", || async {
            apply_decisions(turn.workspace_id, turn.session_id, turn.turn_seq, &decisions).await
        }).await?;

        ctx.set(&key, true).await?;
        Ok(())
    }
}
```

### 5c Step implementations (skeletons)

```rust
fn chunk_turn(turn: &SessionTurn) -> Vec<TurnChunk> {
    // 700-token chunks, semantic boundaries, never split fenced code blocks
    moa_brain::chunking::semantic_chunks(&turn.transcript, 700, 100)
}

async fn extract_facts(chunks: &[TurnChunk]) -> anyhow::Result<Vec<ExtractedFact>> {
    // LLM call with structured output (gpt-4o-mini default; cached on chunk hash)
    // Returns: { entity_uids[], facts[{subject, predicate, object, summary, source_chunk}] }
    todo!()
}

async fn classify_facts(facts: &[ExtractedFact]) -> anyhow::Result<Vec<ClassifiedFact>> {
    let pii = OpenAiPrivacyFilterClassifier::new(env::var("MOA_PII_URL")?);
    let mut out = Vec::with_capacity(facts.len());
    for f in facts {
        let r = pii.classify(&f.summary).await?;
        out.push(ClassifiedFact { fact: f.clone(), pii_class: r.class, pii_spans: r.spans });
    }
    Ok(out)
}

async fn embed_batch(classified: &[ClassifiedFact]) -> anyhow::Result<Vec<EmbeddedFact>> {
    let emb = CohereV4Embedder::new(env::var("COHERE_API_KEY")?);
    let texts: Vec<String> = classified.iter().map(|c| c.fact.summary.clone()).collect();
    let vecs = emb.embed(&texts).await?;
    Ok(classified.iter().zip(vecs).map(|(c, v)| EmbeddedFact { classified: c.clone(), embedding: v }).collect())
}

async fn detect_contradictions(_embedded: &[EmbeddedFact]) -> anyhow::Result<Vec<IngestDecision>> {
    // M12 implements; for now return Insert for everything
    todo!()
}

async fn apply_decisions(
    workspace_id: Uuid, session_id: Uuid, turn_seq: i64, decisions: &[IngestDecision],
) -> anyhow::Result<()> {
    let store = global_graph_store();
    for d in decisions {
        let fact_hash = blake3::hash(d.summary.as_bytes()).as_bytes().to_vec();
        // dedup
        let already = sqlx::query_scalar!(
            "SELECT fact_uid FROM moa.ingest_dedup WHERE workspace_id=$1 AND session_id=$2 AND turn_seq=$3 AND fact_hash=$4",
            workspace_id, session_id, turn_seq, &fact_hash
        ).fetch_optional(&pool).await?;
        if already.is_some() { continue; }

        match d {
            IngestDecision::Insert(intent) => {
                let uid = store.create_node(intent.clone()).await?;
                sqlx::query!("INSERT INTO moa.ingest_dedup (workspace_id, session_id, turn_seq, fact_hash, fact_uid) VALUES ($1,$2,$3,$4,$5)",
                             workspace_id, session_id, turn_seq, &fact_hash, uid).execute(&pool).await?;
            }
            IngestDecision::Supersede { old_uid, new_intent } => {
                store.supersede_node(*old_uid, new_intent.clone()).await?;
            }
            IngestDecision::SkipDuplicate => {}
        }
    }
    Ok(())
}
```

### 5d Backpressure check

In `IngestionVO::ingest_turn` first-line:

```rust
if let Some(ws_state) = sqlx::query!("SELECT slow_path_degraded FROM moa.workspace_state WHERE workspace_id = $1", turn.workspace_id)
    .fetch_optional(&pool).await? {
    if ws_state.slow_path_degraded && rand::random::<f32>() > 0.5 {
        // sample 50% during degraded mode for non-PHI facts
        if turn.dominant_pii_class == "none" { return Ok(()); }
    }
}
```

A separate scheduled job checks queue depth and toggles the flag.

### 5e Session-emit hook

`moa-session` (or wherever turns finalize) calls:

```rust
restate_client.object::<IngestionVOClient>()
    .key(&format!("{}:{}", turn.workspace_id, turn.session_id))
    .ingest_turn(&turn).send().await?;
```

at session-turn finalization.

## 6 Deliverables

- `migrations/M10_ingest.sql` (~80 lines).
- `crates/moa-orchestrator/src/ingestion_vo.rs` (~400 lines).
- Hook in `moa-session::turn_finalized`.
- Restate service registration in `moa-runtime` startup.

## 7 Acceptance criteria

1. End-to-end: emit a fake turn with 3 facts, all 3 land as `Fact` nodes within 60 seconds.
2. Re-emit the same turn — no duplicate nodes (idempotency).
3. Kill the Restate service mid-turn; restart; turn completes.
4. PII-bearing fact lands with `pii_class != 'none'`.
5. `slow_path_degraded=true` reduces ingestion rate of low-PII facts.

## 8 Tests

```sh
cargo run --bin moa-runtime &
cargo test -p moa-orchestrator --test ingestion_e2e
cargo test -p moa-orchestrator --test ingestion_idempotent
```

## 9 Cleanup

- Old wiki "auto-write to MEMORY.md" path in `moa-brain` is now redundant; delete the function and the cron job that scheduled it.
- Remove any direct `MemoryStore::write_*` calls from session finalization — they all route through the VO now.

## 10 What's next

**M11 — Fast-path ingestion (`remember`/`forget`/`supersede` with <500ms target).**

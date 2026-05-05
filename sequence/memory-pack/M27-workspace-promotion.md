# Step M27 — Workspace promotion mechanism (pgvector → Turbopuffer with dual-read window)

_Migrate a workspace from pgvector to Turbopuffer with a dual-read window so retrieval keeps working during the cutover._

## 1 What this step is about

Some workspaces grow past pgvector's HNSW sweet spot. Promoting them to Turbopuffer requires copying every embedding, validating, then atomically flipping the `workspace_state.vector_backend` flag — without dropping retrieval availability mid-migration.

## 2 Files to read

- M05 VectorStore + Pgvector
- M26 TurbopufferStore
- M15 HybridRetriever
- M17 cache invalidation

## 3 Goal

`moa promote-workspace --workspace <wid> --to turbopuffer [--validate-percent 5] [--dual-read-hours 24]` CLI subcommand:

1. Set `workspace_state.vector_backend_state='migrating'`.
2. Stream all `moa.embeddings` rows for ws to Turbopuffer (batch=256).
3. Sample-validate `validate_percent` of vectors (KNN both backends, compare top-K overlap >0.95).
4. Set `vector_backend='turbopuffer'`, `vector_backend_state='dual_read'`, `dual_read_until=NOW()+interval`.
5. During dual-read window, both backends queried; results compared via metrics; pgvector still receives writes.
6. After window: writes-to-pgvector OFF; pgvector partition for ws can be DROPped (separate tooling, not this prompt).

## 4 Rules

- **Cutover is reversible** during dual-read window.
- **Failed validation** → rollback, mark workspace 'pgvector', emit alert.
- **Dual-read fan-out** happens inside HybridRetriever's vector leg behind a feature flag based on workspace_state.
- **Cache (M17) invalidated** on backend flip.
- **Resumable**: copier uses `FOR UPDATE SKIP LOCKED` and tracks `last_uid` so a crashed migration restarts cleanly.

## 5 Tasks

### 5a Migration

Columns already exist from M06 (`vector_backend_state`, `dual_read_until`); ensure index for fast lookups:

```sql
CREATE INDEX IF NOT EXISTS workspace_state_dual_read_idx
  ON moa.workspace_state (vector_backend_state) WHERE vector_backend_state != 'steady';
```

### 5b CLI in `moa-cli/src/commands/admin.rs`

```rust
#[derive(Subcommand)]
pub enum AdminCmd {
    PromoteWorkspace {
        #[arg(long)] workspace: Uuid,
        #[arg(long, default_value = "turbopuffer")] to: String,
        #[arg(long, default_value_t = 5)]   validate_percent: u32,
        #[arg(long, default_value_t = 24)]  dual_read_hours: u32,
    },
    RollbackPromotion { #[arg(long)] workspace: Uuid },
    FinalizePromotion { #[arg(long)] workspace: Uuid }, // post dual-read
}
```

### 5c Streaming copier

```rust
async fn copy_workspace(ws: Uuid, ctx: &Ctx) -> Result<usize> {
    let mut copied = 0;
    loop {
        let batch: Vec<EmbeddingRow> = sqlx::query_as!(
            EmbeddingRow,
            "SELECT * FROM moa.embeddings WHERE workspace_id = $1 AND uid > $2
             ORDER BY uid LIMIT 256 FOR UPDATE SKIP LOCKED", ws, last_uid
        ).fetch_all(&ctx.pool).await?;
        if batch.is_empty() { break; }
        ctx.tp.upsert(&batch.iter().map(VectorItem::from).collect::<Vec<_>>()).await?;
        last_uid = batch.last().unwrap().uid;
        copied += batch.len();
    }
    Ok(copied)
}
```

### 5d Validation harness

Sample 5% by random; compute top-10 overlap; require ≥0.95 average:

```rust
async fn validate(ws: Uuid, sample_pct: f64, ctx: &Ctx) -> Result<f64> {
    let queries = sqlx::query_as!(VectorRow, "SELECT * FROM moa.embeddings WHERE workspace_id = $1 TABLESAMPLE BERNOULLI($2)",
                                  ws, sample_pct).fetch_all(&ctx.pool).await?;
    let mut overlaps = vec![];
    for q in queries {
        let pg_hits = ctx.pg.knn(&q.as_query(10)).await?;
        let tp_hits = ctx.tp.knn(&q.as_query(10)).await?;
        let pg_set: HashSet<_> = pg_hits.iter().map(|h| h.uid).collect();
        let tp_set: HashSet<_> = tp_hits.iter().map(|h| h.uid).collect();
        overlaps.push(pg_set.intersection(&tp_set).count() as f64 / 10.0);
    }
    Ok(overlaps.iter().sum::<f64>() / overlaps.len() as f64)
}
```

### 5e Dual-read shim in HybridRetriever

```rust
async fn vector_leg(&self, req: &RetrievalRequest) -> Result<Vec<(Uuid, f64)>> {
    let state: String = /* fetch workspace_state.vector_backend_state */;
    if state == "dual_read" {
        let (pg_hits, tp_hits) = tokio::join!(self.pg.knn(...), self.tp.knn(...));
        let pg_uids: HashSet<_> = pg_hits.as_ref().map(|h| h.iter().map(|m| m.uid).collect()).unwrap_or_default();
        let tp_uids: HashSet<_> = tp_hits.as_ref().map(|h| h.iter().map(|m| m.uid).collect()).unwrap_or_default();
        let intersection = pg_uids.intersection(&tp_uids).count();
        metrics::histogram!("moa_vector_dualread_overlap", intersection as f64 / 10.0);
        // Use tp result (target backend); fall back to pg on tp failure
        return tp_hits.or(pg_hits).map(rrf_rank);
    }
    /* normal single-backend path */
}
```

### 5f Rollback and finalize

```rust
fn rollback_promotion(ws: Uuid) -> Result<()> {
    sqlx::query!("UPDATE moa.workspace_state SET vector_backend = 'pgvector', vector_backend_state = 'steady', dual_read_until = NULL WHERE workspace_id = $1", ws)
        .execute(pool).await?;
    // Optionally delete the partial Turbopuffer namespace
    Ok(())
}

fn finalize_promotion(ws: Uuid) -> Result<()> {
    let cur: String = sqlx::query_scalar!("SELECT vector_backend_state FROM moa.workspace_state WHERE workspace_id = $1", ws).fetch_one(pool).await?;
    if cur != "dual_read" { return Err(anyhow!("not in dual_read state")); }
    sqlx::query!("UPDATE moa.workspace_state SET vector_backend_state = 'steady', dual_read_until = NULL WHERE workspace_id = $1", ws)
        .execute(pool).await?;
    // Subsequent: pgvector partition for ws may be DROPped via separate maintenance
    Ok(())
}
```

## 6 Deliverables

- `crates/moa-cli/src/commands/admin.rs` (~300 lines).
- `crates/moa-memory-vector/src/promotion.rs` (~250 lines).
- Updated `HybridRetriever::vector_leg`.
- `docs/operations/workspace-promotion-runbook.md`.

## 7 Acceptance criteria

1. End-to-end promotion of 100K-vector workspace completes <10min.
2. Zero retrieval errors during cutover.
3. Top-K overlap ≥ 0.95 across both backends on validation set.
4. Rollback works mid-dual-read.
5. Cache invalidated on backend flip (M17 changelog version bumped).

## 8 Tests

```sh
cargo test -p moa-cli promote_workspace_e2e
cargo test -p moa-memory-vector dual_read_overlap
cargo test -p moa-cli rollback_mid_dual_read
```

## 9 Cleanup

NONE in this phase. pgvector partition drop for promoted workspaces is operator-driven and out-of-scope.

## 10 What's next

**M28 — DELETE entire `moa-memory` crate; final wiki-code sweep.**

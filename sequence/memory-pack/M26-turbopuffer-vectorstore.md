# Step M26 — Turbopuffer `VectorStore` impl (thin reqwest client; no official Rust SDK)

_Add Turbopuffer as a cloud opt-in alternative to pgvector for workspaces that exceed the local HNSW comfort zone (~10M+ vectors per workspace) or require namespace-per-workspace SOC2/HIPAA isolation guarantees Turbopuffer offers natively._

## 1 What this step is about

Turbopuffer (April 2026: SOC 2 Type 2, HIPAA BAA on Scale/Enterprise, BYOC GA) is the right tool when pgvector hits its scale limits. There is no official Rust SDK; we ship a thin reqwest client behind the same `VectorStore` trait from M05. Per-workspace = per-namespace. KEK + per-fact DEK envelope encryption from M21 still applies (Turbopuffer never sees plaintext).

## 2 Files to read

- M05 VectorStore trait
- Turbopuffer HTTP API docs (current as of April 2026)
- M13 moa-memory-vector crate scaffold

## 3 Goal

`TurbopufferStore` impl in `moa-memory/vector/src/turbopuffer.rs` exposing the same `VectorStore` trait as `PgvectorStore`. Configurable per-workspace via `workspace_state.vector_backend ∈ {'pgvector','turbopuffer'}` (default pgvector).

## 4 Rules

- **HTTP/2 keepalive, gzip, retry-with-jitter on 429/5xx** (5 attempts).
- **Namespace name** = `moa-{env}-{workspace_id}`.
- **Vectors uploaded as f32** (Turbopuffer doesn't accept halfvec); convert before send.
- **BAA required for HIPAA workspaces** — gate on `workspace_state.hipaa_tier='hipaa'`.
- **API key per env**, never per workspace (Turbopuffer uses namespaces for isolation).
- **`upsert_in_tx` / `delete_in_tx` return errors** — Turbopuffer is post-commit; the orchestrator handles compensation (M27 dual-read window).

## 5 Tasks

### 5a Cargo deps

```toml
# crates/moa-memory/vector/Cargo.toml
[dependencies]
reqwest = { workspace = true, features = ["json", "rustls-tls", "gzip"] }
backon = "1"
```

### 5b Client struct

```rust
pub struct TurbopufferStore {
    client: reqwest::Client,
    base_url: String,
    api_key: secrecy::SecretString,
    env: String,
}

impl TurbopufferStore {
    pub fn from_env() -> anyhow::Result<Self> { /* read TURBOPUFFER_API_KEY */ }

    fn ns(&self, workspace: Uuid) -> String { format!("moa-{}-{}", self.env, workspace) }

    async fn ensure_namespace(&self, ns: &str) -> anyhow::Result<()> {
        // PUT /v1/namespaces/{ns}; idempotent
    }
}
```

### 5c Implement trait

```rust
#[async_trait::async_trait]
impl VectorStore for TurbopufferStore {
    fn backend(&self) -> &'static str { "turbopuffer" }
    fn dimension(&self) -> usize { 1024 }

    async fn upsert(&self, items: &[VectorItem]) -> anyhow::Result<()> {
        // Group items by workspace; one POST per namespace
        let mut groups: HashMap<Uuid, Vec<&VectorItem>> = HashMap::new();
        for it in items { groups.entry(it.workspace_id.unwrap_or(Uuid::nil())).or_default().push(it); }
        for (ws, batch) in groups {
            let ns = self.ns(ws);
            self.ensure_namespace(&ns).await?;
            let body = json!({
                "upserts": batch.iter().map(|i| json!({
                    "id": i.uid.to_string(),
                    "vector": i.embedding,
                    "attributes": {
                        "label": i.label,
                        "pii_class": i.pii_class,
                        "valid_to": i.valid_to.map(|t| t.to_rfc3339()),
                        "scope": if i.user_id.is_some() {"user"} else {"workspace"},
                    },
                })).collect::<Vec<_>>(),
            });
            self.client.post(format!("{}/v1/namespaces/{}/upsert", self.base_url, ns))
                .bearer_auth(self.api_key.expose_secret())
                .json(&body).send().await?.error_for_status()?;
        }
        Ok(())
    }

    async fn knn(&self, q: &VectorQuery) -> anyhow::Result<Vec<VectorMatch>> {
        // workspace must be present in query — extend VectorQuery if not
        let ns = self.ns(q.workspace_id.expect("Turbopuffer requires explicit workspace"));
        let body = json!({
            "vector": q.embedding,
            "top_k": q.k,
            "filters": filter_expr(q),
        });
        let resp: serde_json::Value = self.client
            .post(format!("{}/v1/namespaces/{}/query", self.base_url, ns))
            .bearer_auth(self.api_key.expose_secret())
            .json(&body).send().await?.error_for_status()?.json().await?;
        Ok(parse_matches(resp))
    }

    async fn delete(&self, uids: &[Uuid]) -> anyhow::Result<()> {
        // batch by namespace; need workspace per uid → look up via sidecar
        unimplemented!("delete needs per-uid namespace resolution; see M27")
    }
}
```

### 5d Schema migration

`migrations/M26_vector_backend.sql` (cosmetic — column was added in M06):

```sql
-- workspace_state.vector_backend already has CHECK ('pgvector','turbopuffer'); no change.
-- Document constraint here for clarity.
```

### 5e Backend selection

`crates/moa-memory/vector/src/backend.rs`:

```rust
pub async fn vector_store_for_workspace(ws: Uuid, pool: &PgPool, pg: Arc<PgvectorStore>, tp: Option<Arc<TurbopufferStore>>) -> Arc<dyn VectorStore> {
    let backend: String = sqlx::query_scalar!("SELECT vector_backend FROM moa.workspace_state WHERE workspace_id = $1", ws)
        .fetch_optional(pool).await.unwrap().flatten().unwrap_or_else(|| "pgvector".into());
    match backend.as_str() {
        "turbopuffer" => tp.unwrap_or_else(|| pg.clone() as _),
        _             => pg,
    }
}
```

### 5f Retries

Use `backon` for exponential backoff with jitter on 429/5xx; max 5 attempts; budget ~3s.

## 6 Deliverables

- `crates/moa-memory/vector/src/turbopuffer.rs` (~450 lines).
- `crates/moa-memory/vector/src/backend.rs`.
- `migrations/M26_vector_backend.sql` (mostly comments).
- Integration test gated on `TURBOPUFFER_API_KEY` env var.

## 7 Acceptance criteria

1. `cargo test -p moa-memory-vector turbopuffer_round_trip` passes against staging when env set.
2. P95 KNN latency <50ms at k=10 for a 1M-vector namespace.
3. Per-workspace BAA gate enforced (workspace marked `hipaa` requires `tp.has_baa()`).
4. Same `VectorStore` interface — caller code unchanged.
5. Backend selection resolves correctly per workspace.

## 8 Tests

```sh
cargo test -p moa-memory-vector turbopuffer_round_trip
cargo test -p moa-memory-vector turbopuffer_namespace_auto_create
cargo test -p moa-memory-vector turbopuffer_429_retry
```

## 9 Cleanup

NONE new (additive backend). Confirm no orphan code from earlier multi-backend explorations remains.

## 10 What's next

**M27 — Workspace promotion (pgvector → Turbopuffer with dual-read window).**

# Step C04 — Migrate `moa-skills` and remaining tail consumers

_Migrate `moa-skills` (notably `regression.rs` and `run_skill_suite`), then sweep `moa-eval`, `moa-runtime`, `moa-gateway`, `moa-loadtest`, and any other crate that C01's inventory flagged. After this prompt, only `moa-core` and `moa-memory` itself reference the legacy types — both are cleaned up in C05/C06._

## 1 What this step is about

Three consumer clusters remain after C02 (CLI) and C03 (orchestrators):

1. **`moa-skills`** — has the heaviest non-orchestrator dependency. `run_skill_suite` takes a `FileMemoryStore` and `regression.rs` exercises wiki behavior end-to-end.
2. **`moa-eval`, `moa-runtime`, `moa-gateway`, `moa-loadtest`** — the tail. C01 inventory will tell you which of these actually import `moa-memory`. Most likely: `moa-eval` for fixture seeding, `moa-runtime` for built-in tool wiring, the others may be clean.
3. **Built-in tools in `moa-skills`** — any `BuiltInTool::execute` that reads `ctx.memory_store` needs migration. The C03 `DeadMemoryStoreShim` will make these fail loudly if missed.

After this prompt, the only places `moa_memory::*` or `moa_core::MemoryStore` still appear are: the legacy crate itself (deleted in C06), and the trait/types definition in `moa-core` (deleted in C05).

## 2 Files to read

- `docs/migrations/moa-memory-inventory.md` — every row not yet handled by C02/C03.
- `crates/moa-skills/src/lib.rs` and `crates/moa-skills/src/regression.rs`.
- `crates/moa-skills/src/builtin/` — every built-in tool whose `execute` reads `ctx.memory_store`.
- `crates/moa-eval/`, `crates/moa-runtime/`, `crates/moa-gateway/`, `crates/moa-loadtest/` — only the files C01 flagged.
- C03's `DeadMemoryStoreShim` and the new orchestrator construction sites (you're calling into the same APIs).

## 3 Goal

After C04:

- `run_skill_suite` accepts `Arc<dyn GraphStore>` instead of `Arc<FileMemoryStore>`.
- `moa-skills/src/regression.rs` uses graph-native fixtures and assertions.
- Every built-in tool that read `ctx.memory_store` either reads `ctx.graph_store` (a new field; bridged in C03 if needed, formalized in C05) or is rewritten to call its own graph-stack handle.
- `moa-eval`, `moa-runtime`, `moa-gateway`, `moa-loadtest` are clean: no `moa_memory::*` imports, no `moa-memory` Cargo dep.
- All workspace tests build. They may not all pass yet (C05 still has the `MemoryStore` trait around for the bridge), but `cargo build --workspace` is clean.

## 4 Rules

- **Skill regression suite is a contract.** If C01 said specific regression cases test wiki behavior with no graph counterpart, retire those cases explicitly with a comment; don't silently drop them.
- **Built-in tools are user-visible surface.** Any tool whose schema or behavior changes (e.g., a `read_memory` tool that took a path now takes a uid) needs a JSON schema update and a migration note.
- **Tail crates: minimum diff.** If `moa-eval` only imports `MemoryStore` for one fixture builder, just rewrite that builder. Don't refactor the whole crate.
- **No new `DeadMemoryStoreShim` instances.** C03 introduced one bridge. C04 doesn't add more — call the graph stack directly.

## 5 Tasks

### 5a `run_skill_suite` signature change

Change in `crates/moa-skills/src/lib.rs`:

```rust
// before
pub async fn run_skill_suite(
    config: &MoaConfig,
    memory_store: Arc<FileMemoryStore>,
    workspace_id: &WorkspaceId,
    skill: &str,
) -> Result<SkillRun>;

// after
pub async fn run_skill_suite(
    config: &MoaConfig,
    graph_store: Arc<dyn GraphStore>,
    workspace_id: &WorkspaceId,
    skill: &str,
) -> Result<SkillRun>;
```

Inside, every `memory_store.search(...)`, `memory_store.read_page(...)`, etc. is replaced by the corresponding graph-stack call. Reads typically become hybrid retriever calls or `GraphStore::get_node`; writes become `IngestionVO::ingest_turn` or `fast_remember`.

If `run_skill_suite` needs the retriever or VO too (not just the raw store), expand the signature:

```rust
pub async fn run_skill_suite(
    config: &MoaConfig,
    graph: SkillGraphHandles,
    workspace_id: &WorkspaceId,
    skill: &str,
) -> Result<SkillRun>;

pub struct SkillGraphHandles {
    pub store: Arc<dyn GraphStore>,
    pub retriever: Arc<HybridRetriever>,
    pub ingestion: Arc<IngestionVO>,
}
```

The CLI's `handle_eval_skill` (touched in C02) also updates to construct and pass `SkillGraphHandles`.

### 5b Rewrite `regression.rs`

The skill regression harness is the densest legacy consumer. It seeds wiki pages, runs skills, asserts wiki-page state changed.

For each regression case:

1. **Seed step**: replace `store.write_page(...)` with `IngestionVO::ingest_turn` or direct `GraphStore::create_node` for compact fixtures.
2. **Run step**: unchanged — the skill executes against whatever store was injected.
3. **Assert step**: replace `store.read_page(...)` / `store.list_pages(...)` with `GraphStore::get_node`, `lookup_seeds`, `neighbors`, or retriever queries against expected hits.

Pattern:

```rust
// before
let topic_path = MemoryPath::new("topics/oauth.md");
store.write_page(&scope, &topic_path, fixture_page("OAuth", ...)).await?;
run_skill(...).await?;
let updated = store.read_page(&scope, &topic_path).await?;
assert!(updated.content.contains("OAuth 2.1"));

// after
let oauth_uid = create_fixture_concept(&store, &scope, "OAuth", &["protocol"]).await?;
run_skill(...).await?;
let updated = store.get_node(oauth_uid).await?.expect("oauth node");
assert!(updated.properties.get("description").and_then(|v| v.as_str()).unwrap_or("").contains("OAuth 2.1"));
```

Where `create_fixture_concept` is a new helper at the top of `regression.rs` that wraps `GraphStore::create_node` with sensible defaults for tests.

### 5c Built-in tool migration

Find every `impl BuiltInTool` whose `execute` reads `ctx.memory_store`:

```sh
rg "ctx\\.memory_store" crates/moa-skills/src/builtin/ --type rust -l
```

For each, decide:

- **`memory_search` tool**: route through retriever. Schema may stay the same (string query → list of hits) but hit shape changes.
- **`memory_read` tool** (if it took a path): retire or redesign to take a uid. Update JSON schema + tool description.
- **`memory_remember` / `memory_write` tool**: route through `fast_remember` for short observations, or `IngestionVO::ingest_turn` for longer text. Update schema.

Each tool now needs its own graph-stack handles. Either:

- pass them through `ToolContext` (C05 will add `graph_store`, `retriever`, `ingestion` fields), or
- have each tool construct its own handles from a shared `Arc<MoaConfig>` carried in `ToolContext` (simpler, slightly more allocation).

Recommended: add the handles to `ToolContext` in C05. For C04, each migrated tool stores its handles as struct fields populated at registration time. Example:

```rust
pub struct MemorySearchTool {
    retriever: Arc<HybridRetriever>,
}

#[async_trait]
impl BuiltInTool for MemorySearchTool {
    async fn execute(&self, input: &Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let query = input["query"].as_str().context("query required")?;
        let scope = MemoryScope::Workspace { workspace_id: ctx.session.workspace_id.clone() };
        let hits = self.retriever.retrieve(query, &scope, 10).await?;
        // serialize hits and return
        Ok(ToolOutput::json(serde_json::to_value(hits)?))
    }
    // ...
}
```

### 5d Tail crates: per-crate sweeps

Repeat for each crate C01 flagged. Typical patterns:

**`moa-eval`**: probably uses `FileMemoryStore` to seed eval fixtures.

```rust
// in eval suite setup
let store = FileMemoryStore::from_config(&config).await?;
seed_eval_pages(&store, &fixtures).await?;
```

becomes:

```rust
let graph = AgeGraphStore::from_config_with_pool(&config, pool, schema).await?;
let ingestion = IngestionVO::new(Arc::new(graph), embedder, pii).await?;
seed_eval_graph(&ingestion, &fixtures).await?;
```

**`moa-runtime`**: probably wires the `MemoryStore` trait into the global runtime context. Replace with `GraphStore` wiring. May require touching the runtime config struct.

**`moa-gateway`**: less likely to use memory directly — probably only via `BrainOrchestrator`. If clean, no change.

**`moa-loadtest`**: probably has fixture seeding similar to `moa-eval`. Same treatment.

### 5e Update each tail crate's `Cargo.toml`

Remove `moa-memory = { path = ... }`. Add the graph stack as needed:

```toml
moa-memory-graph = { path = "../moa-memory-graph" }
moa-memory-ingest = { path = "../moa-memory-ingest" }
moa-brain = { path = "../moa-brain" }   # only if the crate calls into the retriever
```

### 5f Migration notes

For each non-trivial behavior change, add a one-line note in `docs/migrations/moa-memory-inventory.md` under that consumer's row:

> **Migration**: regression case `oauth_token_refresh` retired — wiki-only assertion path.

This keeps the audit trail explicit.

## 6 Deliverables

- `run_skill_suite` signature changed; `regression.rs` rewritten against graph fixtures.
- Every built-in tool that touched `ctx.memory_store` migrated (via the bridge or via direct struct fields).
- Each tail crate (`moa-eval`, `moa-runtime`, `moa-gateway`, `moa-loadtest`) updated as needed.
- Cargo.tomls cleaned.
- Migration notes appended to the inventory doc.

## 7 Acceptance criteria

1. `rg "use moa_memory" crates/moa-skills/ crates/moa-eval/ crates/moa-runtime/ crates/moa-gateway/ crates/moa-loadtest/` returns 0 hits.
2. `rg "FileMemoryStore" crates/moa-skills/ crates/moa-eval/ crates/moa-runtime/ crates/moa-gateway/ crates/moa-loadtest/` returns 0 hits.
3. `rg "moa-memory\\s*=" crates/*/Cargo.toml | grep -v moa-memory-` returns 0 hits (only the four subcrates `moa-memory-graph|vector|pii|ingest` should remain).
4. `cargo build --workspace` clean.
5. `cargo test -p moa-skills` green.
6. The skill regression suite still has the same number of cases (or fewer with retired cases explicitly marked).
7. Built-in `memory_*` tools work end-to-end against a test DB.

## 8 Tests

```sh
cargo build --workspace
cargo test -p moa-skills
cargo test -p moa-eval
cargo test -p moa-runtime
cargo test -p moa-gateway
cargo test -p moa-loadtest

# Skill regression smoke:
moa eval skill <some-skill> --ci
```

## 9 Cleanup

- `cargo fmt && cargo clippy --workspace -- -D warnings` clean.
- Audit the inventory doc and confirm every row's "Migration outcome" column is filled in.
- Confirm `DeadMemoryStoreShim` from C03 is the only place `MemoryStore::*` methods are still implemented in the workspace (it's transient — deleted in C05).

## 10 What's next

**C05 — Delete `MemoryStore` trait and wiki types from `moa-core`.** The trait and types now have zero in-workspace consumers; they're safe to remove.

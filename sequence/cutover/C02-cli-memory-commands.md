# Step C02 — Reshape `moa-cli` memory commands

_Replace `moa-cli`'s wiki-shaped memory commands with graph-shaped ones, retire the commands that have no graph equivalent. The `moa memory` subcommand surface stays user-facing but its semantics now match the graph subsystem._

## 1 What this step is about

Today `moa-cli` exposes five memory subcommands that all sit on `FileMemoryStore` / the `MemoryStore` trait:

- `moa memory search <query>` — FTS over wiki markdown.
- `moa memory show <path>` — read one wiki page by logical path.
- `moa memory ingest <files>` — drops raw documents into the wiki via `ingest_source`.
- `moa memory rebuild-index` — rebuilds the FTS sidecar from markdown files.
- `moa memory rebuild-embeddings` — already a no-op shim.

In the graph world:

- `search` becomes a hybrid retriever call (M15) that returns ranked nodes.
- `show` either retires (no graph equivalent of "page by path") or redesigns to take a node uid.
- `ingest` routes through the slow-path Ingestion VO (M10) which turns a raw doc into nodes, edges, and embeddings.
- `rebuild-index` and `rebuild-embeddings` retire entirely — the graph subsystem maintains its indexes incrementally on writes; no rebuild step exists.

The `moa doctor` report's `memory_index` / `memory_embeddings` lines also need to be reshaped because they currently call `MemoryStore::get_index` and the legacy embedding status.

## 2 Files to read

- `docs/migrations/moa-memory-inventory.md` (output of C01 — has the user's Decisions for `WIKI_RETIRE` rows).
- `crates/moa-cli/src/main.rs` (every `MemoryCommand` arm and the `memory_*_report` helpers).
- `crates/moa-memory-graph/src/lib.rs` (the `GraphStore` trait you'll call into).
- `crates/moa-memory-ingest/src/lib.rs` (the `IngestionVO` API for `ingest`).
- `crates/moa-brain/src/pipeline/` — find the hybrid retriever entry point built in M15.
- C01's inventory rows for `moa-cli/src/main.rs`.

## 3 Goal

After this step:

- `moa memory search <query>` calls the hybrid retriever and prints ranked node hits (one per line: `uid`, `label`, `name`, `score`, snippet).
- `moa memory show <uid>` (renamed from `show <path>`) fetches one node from the sidecar and prints its properties + neighbors. Or: the command is retired entirely, depending on the C01 decision.
- `moa memory ingest <files>` packs each file into a synthetic `SessionTurn` and calls `IngestionVO::ingest_turn`, then prints the ingestion summary (nodes created, edges created, contradictions detected).
- `moa memory rebuild-index` and `moa memory rebuild-embeddings` are deleted (subcommands removed from `MemoryCommand` enum, helpers deleted, tests deleted).
- `moa doctor` reports the graph subsystem's health (sidecar row count, recent write timestamp) instead of the wiki index.
- `handle_eval_skill` (which calls `load_memory_store`) is updated to load a `GraphStore` handle instead of `FileMemoryStore`, in coordination with C04 which will update `run_skill_suite`'s signature.
- `crates/moa-cli/Cargo.toml` no longer depends on `moa-memory`. It depends on `moa-memory-graph`, `moa-memory-ingest`, `moa-brain` instead.

## 4 Rules

- **C01 decisions are binding.** If the inventory says retire a command, retire it; if it says redesign, redesign it as specified. Don't second-guess.
- **Help text matches reality.** Every retained subcommand's `about = ...` clap doc string is rewritten to describe graph behavior, not wiki behavior.
- **Tests follow the code.** Delete tests for retired commands. Rewrite tests for redesigned commands against the new API.
- **No FileMemoryStore in the new code.** Not in `main.rs`, not in tests, not in any helper.
- **`moa-cli/Cargo.toml`** swaps `moa-memory` for the graph stack.

## 5 Tasks

### 5a Replace `MemoryCommand` enum

Remove retired arms (per C01 decisions; default assumption is `Show` redesigns to `<uid>`, `RebuildIndex` and `RebuildEmbeddings` retire). New shape:

```rust
#[derive(Debug, Subcommand)]
enum MemoryCommand {
    /// Searches workspace memory using hybrid graph + vector retrieval.
    Search {
        /// Search query.
        query: String,
        /// Maximum number of hits to return.
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Displays one memory node by uid, with its immediate neighbors.
    Show {
        /// Node uid (UUIDv7).
        uid: String,
    },
    /// Ingests one or more documents into workspace memory through the slow-path pipeline.
    Ingest(IngestArgs),
}
```

### 5b Replace `memory_search_report`

```rust
async fn memory_search_report(config: &MoaConfig, query: &str, limit: usize) -> Result<String> {
    let scope = MemoryScope::Workspace { workspace_id: current_workspace_id() };
    let retriever = load_hybrid_retriever(config).await?;
    let hits = retriever.retrieve(query, &scope, limit).await?;

    let mut report = String::new();
    if hits.is_empty() {
        report.push_str("no hits\n");
        return Ok(report);
    }
    report.push_str("uid\tlabel\tname\tscore\tsnippet\n");
    for hit in hits {
        report.push_str(&format!(
            "{}\t{}\t{}\t{:.3}\t{}\n",
            hit.uid, hit.label, hit.name, hit.score, hit.snippet
        ));
    }
    Ok(report)
}
```

`load_hybrid_retriever` is a new local helper that wires up the M15 retriever from a `MoaConfig`. The retriever's exact API depends on what M15 produced — adapt the call to match.

### 5c Replace `memory_show_report`

```rust
async fn memory_show_report(config: &MoaConfig, uid_str: &str) -> Result<String> {
    let uid = Uuid::parse_str(uid_str)
        .with_context(|| format!("invalid node uid `{uid_str}`"))?;
    let store = load_graph_store(config).await?;
    let node = store
        .get_node(uid)
        .await?
        .with_context(|| format!("node {uid} not found"))?;
    let neighbors = store.neighbors(uid, 1, None).await.unwrap_or_default();

    let mut report = format!(
        "uid: {}\nlabel: {:?}\nname: {}\nscope: {:?}\nvalid_from: {}\nvalid_to: {}\n\nproperties:\n{}\n",
        node.uid,
        node.label,
        node.name,
        node.scope,
        node.valid_from,
        node.valid_to.map(|t| t.to_rfc3339()).unwrap_or_else(|| "<open>".to_string()),
        serde_json::to_string_pretty(&node.properties)?,
    );
    if !neighbors.is_empty() {
        report.push_str("\nneighbors:\n");
        for n in neighbors {
            report.push_str(&format!("- {} {:?} {}\n", n.uid, n.label, n.name));
        }
    }
    Ok(report)
}
```

If the C01 decision was "retire `show` entirely," skip 5c and just remove the `Show` arm in 5a.

### 5d Replace `memory_ingest_report`

```rust
async fn memory_ingest_report(
    config: &MoaConfig,
    files: &[PathBuf],
    name: Option<&str>,
    workspace: Option<&str>,
) -> Result<String> {
    if files.is_empty() {
        bail!("at least one file path is required");
    }
    if files.len() > 1 && name.is_some() {
        bail!("--name can only be used when ingesting a single file");
    }

    let workspace_id = workspace.map(resolve_workspace_arg).unwrap_or_else(current_workspace_id);
    let scope = MemoryScope::Workspace { workspace_id: workspace_id.clone() };
    let vo = load_ingestion_vo(config).await?;

    let mut sections = Vec::with_capacity(files.len());
    for file in files {
        let content = fs::read_to_string(file).await
            .with_context(|| format!("reading {}", file.display()))?;
        let source_name = name.map(String::from).unwrap_or_else(|| derive_ingest_source_name(file));

        // Synthesize a SessionTurn carrying the document. The VO chunks, extracts entities,
        // embeds, writes nodes and edges, and emits an IngestionReport.
        let turn = synthesize_cli_ingest_turn(&workspace_id, &source_name, &content);
        let report = vo.ingest_turn(&scope, turn).await?;
        sections.push(format_cli_ingest_section(file, &report));
    }

    let mut output = String::new();
    if files.len() > 1 {
        output.push_str(&format!("Ingested {} documents into workspace memory.\n\n", files.len()));
    }
    output.push_str(&sections.join("\n\n"));
    output.push('\n');
    Ok(output)
}
```

`synthesize_cli_ingest_turn` is a new helper that builds a `SessionTurn` from a `(workspace_id, source_name, content)` tuple. It assigns a synthetic session uid (e.g., `session::cli-ingest::<uuid_v7>`) so the audit trail shows the turn came from the CLI.

`format_cli_ingest_section` updates to print graph-shaped fields (nodes created, edges created, contradictions, embedding count) instead of the wiki-shaped `Created: …` / `Updated: N pages` text.

### 5e Delete retired commands

Remove from `main.rs`:

- `MemoryCommand::RebuildIndex` arm.
- `MemoryCommand::RebuildEmbeddings` arm.
- `RebuildIndexArgs`, `RebuildEmbeddingsArgs` structs.
- `memory_rebuild_index_report` function.
- `memory_rebuild_embeddings_report` function.
- `discover_memory_scopes` function (it's only called by the deleted helpers).

### 5f Update `doctor_report`

The `memory_index_status` and `memory_embedding_status` helpers currently call `FileMemoryStore`. Replace with graph health checks:

```rust
async fn graph_memory_status(config: &MoaConfig) -> String {
    match load_graph_store(config).await {
        Ok(store) => {
            // Cheap health check: count rows in the sidecar node index for the current workspace.
            match store.count_nodes_in_workspace(&current_workspace_id()).await {
                Ok(n) => format!("healthy ({n} nodes in current workspace)"),
                Err(e) => format!("unhealthy ({e})"),
            }
        }
        Err(e) => format!("unhealthy ({e})"),
    }
}
```

(`count_nodes_in_workspace` is a thin sidecar-SQL helper to add to `moa-memory-graph` if it doesn't exist; one row, one query.)

In `doctor_report`, replace the two memory lines with a single graph line:

```rust
format!("graph_memory: {}", graph_memory_status(config).await),
```

### 5g Update `handle_eval_skill`

Currently:

```rust
let memory_store = Arc::new(load_memory_store(&config).await?);
let workspace_id = current_workspace_id();
let skill_run = run_skill_suite(&config, memory_store, &workspace_id, &args.skill).await?;
```

Change to:

```rust
let graph_store = Arc::new(load_graph_store(&config).await?);
let workspace_id = current_workspace_id();
let skill_run = run_skill_suite(&config, graph_store, &workspace_id, &args.skill).await?;
```

(`run_skill_suite`'s signature changes in C04. This file currently won't compile until C04 is also done. That's OK — both prompts will land before the build is checked end-to-end.)

### 5h Replace local helpers

Delete `load_memory_store`. Add `load_graph_store`, `load_hybrid_retriever`, `load_ingestion_vo`. Each is a thin wrapper that takes `&MoaConfig` and returns the configured implementation. Pull connection settings from the same Postgres pool helper that `load_session_store` uses.

### 5i Update `moa-cli/Cargo.toml`

Remove:

```toml
moa-memory = { path = "../moa-memory" }
```

Add:

```toml
moa-memory-graph = { path = "../moa-memory-graph" }
moa-memory-ingest = { path = "../moa-memory-ingest" }
moa-brain = { path = "../moa-brain" }
```

(`moa-brain` is added because `load_hybrid_retriever` lives there. If `moa-cli` already depends on `moa-brain`, no change.)

### 5j Update tests

In `mod tests` of `main.rs`:

- Delete `memory_ingest_report_*` tests that use `FileMemoryStore`.
- Add a smoke test that synthesizes one ingestion turn through `IngestionVO` and asserts the report contains "nodes:" and "edges:" lines.
- Add a smoke test for `memory_search_report` against an empty graph (expect "no hits").
- Add a smoke test for `memory_show_report` against a known seeded uid.

These tests need the test database fixtures from M03/M04 (graph schema, sidecar, AGE init). If those fixtures aren't accessible from `moa-cli`'s test harness, gate the new tests behind `#[ignore]` and document that they run via `cargo test --ignored -p moa-cli` against a live test DB.

## 6 Deliverables

- Rewritten `MemoryCommand` enum in `crates/moa-cli/src/main.rs`.
- Three new `memory_*_report` helpers (search/show/ingest) using the graph stack.
- Three retired helpers removed.
- Updated `doctor_report`.
- Updated `handle_eval_skill`.
- Updated `Cargo.toml` deps.
- Updated tests.
- A short blurb in `architecture.md` under the "CLI" section documenting the new command shapes.

## 7 Acceptance criteria

1. `rg "FileMemoryStore" crates/moa-cli/` returns 0 hits.
2. `rg "use moa_memory" crates/moa-cli/` returns 0 hits.
3. `rg "MemoryStore" crates/moa-cli/` returns 0 hits (the `MemoryStore` import is gone).
4. `crates/moa-cli/Cargo.toml` does not list `moa-memory`.
5. `cargo build -p moa-cli` clean (will fail at the workspace level until C03/C04 land if other consumers still call `moa-memory` — that's expected; this prompt's local build is what matters here).
6. `cargo test -p moa-cli --lib` green for the non-ignored tests.
7. `moa memory search "foo"`, `moa memory show <uid>`, `moa memory ingest some.md` all run end-to-end against a test DB.

## 8 Tests

```sh
cargo build -p moa-cli
cargo test -p moa-cli --lib
# Manual smoke (against a configured DB):
moa memory search "test query"
moa memory ingest /tmp/sample.md
```

## 9 Cleanup

- Confirm no dead imports remain in `main.rs` (e.g., stale `MemoryPath` import after `Show` redesign).
- Confirm no `_report` helper is unreachable.
- `cargo fmt && cargo clippy -p moa-cli -- -D warnings` clean.

## 10 What's next

**C03 — Orchestrators.** Migrate `moa-orchestrator-local` (and `moa-orchestrator` if applicable) off `MemoryStore` to the graph stack.

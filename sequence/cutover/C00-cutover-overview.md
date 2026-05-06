# Step C00 — Cutover overview (read-only)

_Read this before running C01. No code changes in this step. It explains why the cutover is sequenced the way it is and what each C-prompt does._

## 1 Why we need a cutover

After M01–M19, the repo has two parallel memory systems:

- **Legacy: `moa-memory` / `FileMemoryStore` / `MemoryStore` trait in `moa-core`.** Stores knowledge as markdown files at logical paths, with a Postgres FTS sidecar. Wiki-shaped operations: `read_page`, `write_page`, `list_pages`, `get_index`, `ingest_source`, `rebuild_search_index`. Used by `moa-cli`, `moa-orchestrator-local`, `moa-skills`, `moa-eval`, others.

- **New: `moa-memory-graph` / `AgeGraphStore` / `GraphStore` trait.** Stores knowledge as graph nodes (Entity, Concept, Decision, Incident, Lesson, Fact, Source) with bi-temporal validity, RLS, sidecar projection, vector embeddings, and a Restate-orchestrated ingestion pipeline. Built by M07–M19. Read paths through the hybrid retriever (M15) + query planner (M16). Write paths through the slow-path VO (M10) + fast-path API (M11).

The two systems coexist. The new one is built and unit-tested; the legacy one is still wired into production paths. None of the M01–M19 prompts explicitly migrated consumers. That's what C01–C06 do.

## 2 The semantic gap

The two APIs aren't equivalent. Wiki operations have no graph counterparts and vice versa:

| Legacy (wiki)                                | New (graph)                                                  |
|----------------------------------------------|--------------------------------------------------------------|
| `read_page(scope, "topics/auth.md")`         | `get_node(uid)` — but uids aren't paths                      |
| `write_page(scope, path, WikiPage)`          | `fast_remember(text)` or `IngestionVO::ingest_turn`          |
| `list_pages(scope, filter=Topic)`            | `lookup_seeds(name, limit)` or label-filtered SQL on sidecar |
| `get_index(scope)` → markdown               | Hybrid retriever query → ranked nodes                        |
| `ingest_source(scope, name, raw_md)`         | `IngestionVO::ingest_turn` (chunks, extracts, embeds)        |
| `rebuild_search_index(scope)`                | (no equivalent — graph indexes maintained by writes)         |
| `write_page_branched` / `reconcile_branches` | (no equivalent — bi-temporal SUPERSEDES handles concurrency) |

So the migration is **semantic**, not mechanical. For each consumer, we ask: "what is this code actually trying to accomplish, and what's the graph-native way to do it?" Some consumers will simplify. A few will lose features the wiki had but the graph doesn't. That's expected.

## 3 Cutover sequence

Six prompts, in order:

- **C01 — Inventory and classification.** Agent runs `rg` to find every consumer of `moa_memory::*` and produces `docs/migrations/moa-memory-inventory.md`. Each consumer is classified into one of:
  - `TRIVIAL_DELETE`: dead code, unused fixtures, deprecated debug.
  - `SIMPLE_GRAPH`: small site, swap to `GraphStore::*` directly.
  - `INGEST_ROUTE`: write site that should route through `IngestionVO` or `fast_remember`.
  - `RETRIEVAL_ROUTE`: read site that should route through hybrid retrieval.
  - `WIKI_RETIRE`: feature has no graph equivalent (e.g., `moa memory show <path>`); decide retire/redesign.
  - `NEEDS_DESIGN`: complex semantics worth a focused decision (e.g., consolidation, branching).
  - **You review the inventory before continuing to C02.** Edit the doc with your decisions for `WIKI_RETIRE` and `NEEDS_DESIGN` rows.

- **C02 — CLI memory commands.** `moa-cli` has the most user-visible legacy memory surface: `moa memory {search, show, ingest, rebuild-index, rebuild-embeddings}`. C02 reshapes them to graph semantics where possible, retires the rest. Updates `moa doctor` and the help text.

- **C03 — Orchestrators.** `moa-orchestrator-local` (and `moa-orchestrator` if it consumes the trait) gets cut over. These are the runtime hot paths. The fast-path API (`moa_memory_ingest::fast_remember`) and slow-path VO (`IngestionVO::ingest_turn`) replace direct `MemoryStore::write_page` / `ingest_source` calls.

- **C04 — Skills + tail.** `moa-skills` (notably `regression.rs` and `run_skill_suite`), plus any remaining consumers in `moa-eval`, `moa-runtime`, `moa-gateway`, `moa-loadtest`. Skill regression fixtures get rewritten against graph-native test data.

- **C05 — Delete trait + types from `moa-core`.** Now that no consumer imports them, delete:
  - `MemoryStore` trait (`crates/moa-core/src/traits.rs`)
  - `WikiPage`, `MemoryPath`, `PageType`, `PageSummary`, `IngestReport`, `MemorySearchMode`, `MemorySearchResult`, `ConfidenceLevel` (in `crates/moa-core/src/types/`)
  - Any helper functions tied to wiki page rendering / parsing.

- **C06 — Delete `moa-memory` crate.** Final removal of `crates/moa-memory/`. Update `Cargo.toml` workspace members. CI guardrail confirms it stays gone.

## 4 What "done" looks like

After C06:

```sh
test ! -d crates/moa-memory                              # gone
rg "moa_memory::" crates/                                # zero hits
rg "use moa_memory" crates/                              # zero hits
rg "FileMemoryStore" crates/                             # zero hits
rg "trait MemoryStore" crates/                           # zero hits
rg "use moa_core::WikiPage|use moa_core::MemoryPath" crates/   # zero hits
cargo build --workspace                                  # clean
cargo test --workspace                                   # green
```

The graph subsystem is the **only** memory system. R01 (folder-grouping the four graph crates under `moa-memory/`) becomes safe and trivial after this point.

## 5 Risks and mitigations

| Risk | Mitigation |
|---|---|
| C02 retires a wiki feature that some user depends on | C01 inventory surfaces this. You decide retire vs. redesign before C02 starts. |
| Graph fixtures don't cover a test the wiki regression had | C04 explicitly rewrites the skill regression suite; if a test drops, document why. |
| `moa-core` types deletion (C05) breaks a consumer C01 missed | C05 starts with a re-audit; if any consumer still imports a deleted type, it's caught before deletion. |
| Long cutover blocks other work for days | Each C-prompt is independently committable and the build stays green between them. You can pause and resume. |
| Wiki data on disk (`~/.moa/memory/...`) becomes orphaned | Document a one-shot migration script (or accept loss for dev environments). Production users should export wiki data via `moa memory show` before C02 if they care. |

## 6 What this prompt produces

Nothing. This is a read-only orientation document. No commits.

## 7 Acceptance criteria

You've read this and understand the sequence. No automated check.

## 8 Tests

None.

## 9 Cleanup

None.

## 10 What's next

**C01 — Consumer inventory.** Agent audits every site that imports `moa_memory::*` and produces a classification table you review.

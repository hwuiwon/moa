# Step 05: File-Wiki Memory Store + FTS5

## What this step is about
Implementing the `MemoryStore` trait as a file-backed wiki with FTS5 search.

## Files to read
- `docs/04-memory-architecture.md` — Complete file structure, page format, FTS5 schema, operations
- `docs/01-architecture-overview.md` — `MemoryStore` trait

## Goal
Read/write markdown wiki pages with YAML frontmatter. Maintain an FTS5 index. Support per-user and per-workspace scoping.

## Rules
- Files are the source of truth — FTS5 is a derived index, rebuildable from files
- YAML frontmatter parsing with `serde_yaml` (or `gray_matter` crate)
- MEMORY.md limited to 200 lines when loaded for context
- `wiki_search` FTS5 table uses porter stemmer + unicode61 tokenizer
- All file paths are relative to the scope root (`~/.moa/memory/` for user, `~/.moa/workspaces/{id}/memory/` for workspace)

## Tasks
1. **`moa-memory/src/wiki.rs`**: `WikiPage` struct (frontmatter + body), parse/serialize markdown with YAML
2. **`moa-memory/src/index.rs`**: MEMORY.md management (read, update, truncate to 200 lines)
3. **`moa-memory/src/fts.rs`**: FTS5 index (create, rebuild, search, update on file change)
4. **`moa-memory/src/lib.rs`**: `FileMemoryStore` implementing `MemoryStore` trait
5. Wire `memory_search` and `memory_read` into pipeline Stage 5 (MemoryRetriever) — update the stub from Step 04

## Deliverables
```
moa-memory/src/
├── lib.rs           # FileMemoryStore
├── wiki.rs          # WikiPage struct + parse/serialize
├── index.rs         # MEMORY.md management
└── fts.rs           # FTS5 search index
```

## Acceptance criteria
1. Can create, read, update, delete wiki pages with YAML frontmatter
2. FTS5 search finds pages by content with ranked results
3. MEMORY.md is auto-truncated to 200 lines when loaded
4. Scoping works: user and workspace memories are separate
5. `rebuild_search_index()` recreates FTS from files
6. Pipeline Stage 5 now loads MEMORY.md and searches for relevant pages

## Tests
- Unit test: WikiPage roundtrip (write → read → compare)
- Unit test: Frontmatter parsing (type, confidence, related, tags)
- Integration test: Write 10 pages, search for keywords, verify ranked results
- Integration test: Rebuild FTS index from scratch, verify search still works
- Unit test: MEMORY.md truncation at 200 lines

```bash
cargo test -p moa-memory
```

---


# Step M13 — Split vector code out of legacy `moa-memory` crate

_Move every vector-related module out of `moa-memory` and into `moa-memory-vector` (already created in M05). After this step, `moa-memory` has no vector code at all — it's purely a deprecation shim until M28 deletes it._

## 1 What this step is about

`moa-memory` originally housed embedder, vector store, similarity-search, and chunking utilities all together. M05 created the dedicated vector crate and added the trait + pgvector impl. This prompt moves the rest of the surface area: chunking, embedder concrete impls, similarity helpers, and any remaining tests. After this prompt, `cargo tree -p moa-memory` shows no vector dependencies (no pgvector, no reqwest-for-cohere, etc.).

## 2 Files to read

- `crates/moa-memory/Cargo.toml`
- `crates/moa-memory/src/lib.rs` (look for `pub mod vector`, `pub mod embedder`, `pub mod chunking`)
- M05 files (the destination)

## 3 Goal

1. All vector-related modules physically moved (`git mv`) from `moa-memory/src/` to `moa-memory-vector/src/`.
2. `moa-memory/Cargo.toml` no longer depends on `pgvector`, `reqwest`, `cohere-related`, `tiktoken`, `pca`, etc.
3. Consumers updated to import from `moa-memory-vector`.
4. `moa-memory/src/lib.rs` becomes a re-export shim with `#[deprecated]` markers pointing at `moa-memory-vector`.

## 4 Rules

- **`git mv` not copy-and-paste.** Preserve history.
- **Public API is preserved through `#[deprecated]` re-exports** for one cycle, so M14/M15 don't have to scramble. M28 deletes the shim entirely.
- **No new functionality** added in this prompt — pure refactor.

## 5 Tasks

### 5a Identify modules

Run:

```sh
rg -l "vector|embed|chunk" crates/moa-memory/src/
ls crates/moa-memory/src/
```

Typical findings: `src/vector.rs`, `src/embedder/{cohere.rs,openai.rs,mock.rs}`, `src/chunking.rs`, `src/similarity.rs`, `src/text/tokenizer.rs`. Confirm against actual repo state.

### 5b Move files

```sh
git mv crates/moa-memory/src/vector.rs       crates/moa-memory-vector/src/legacy_vector.rs
git mv crates/moa-memory/src/embedder        crates/moa-memory-vector/src/embedder
git mv crates/moa-memory/src/chunking.rs     crates/moa-memory-vector/src/chunking.rs
git mv crates/moa-memory/src/similarity.rs   crates/moa-memory-vector/src/similarity.rs
```

(Adjust to actual file inventory.)

### 5c Update Cargo.toml

```toml
# crates/moa-memory/Cargo.toml — REMOVE
# pgvector = ...
# reqwest = ...
# tiktoken-rs = ...
# (any vector-only deps)
```

```toml
# crates/moa-memory-vector/Cargo.toml — ADD what was moved
[dependencies]
async-trait = "0.1"
sqlx = { workspace = true, features = [...] }
pgvector = { version = "0.4", features = ["sqlx", "halfvec"] }
reqwest = { workspace = true, features = ["json", "rustls-tls"] }
tiktoken-rs = "0.6"
unicode-segmentation = "1"
# ... any others that came along
```

### 5d Re-export shim

`crates/moa-memory/src/lib.rs`:

```rust
//! DEPRECATED — moa-memory has been split. See M00 in moa/sequence/.
//!
//! Vector code: `moa_memory_vector::*`
//! Graph code:  `moa_memory_graph::*`
//! PII code:    `moa_memory_pii::*`
//! Ingest code: `moa_memory_ingest::*`

#[deprecated(note = "use moa_memory_vector::Embedder")]
pub use moa_memory_vector::Embedder;
#[deprecated(note = "use moa_memory_vector::CohereV4Embedder")]
pub use moa_memory_vector::CohereV4Embedder;
#[deprecated(note = "use moa_memory_vector::chunking")]
pub mod chunking { pub use moa_memory_vector::chunking::*; }
// ...etc
```

### 5e Consumer sweep

```sh
rg "use moa_memory::" crates/ --type rust
```

For each hit, decide whether to:
- Update to `use moa_memory_vector::...` directly (preferred), or
- Leave on the deprecation shim (acceptable for trivial call sites that will be deleted in M28).

Update at least: `moa-brain` retrieval, `moa-orchestrator` ingestion, `moa-cli` chunk-debug commands. Leave `moa-memory/tests/*` alone — they migrate into `moa-memory-vector/tests/` only if they're vector-specific; otherwise they go away in M28.

### 5f Tests

```sh
cargo build --workspace
cargo test --workspace
cargo +nightly udeps -p moa-memory  # should show many removed deps as fully unused
```

## 6 Deliverables

- File moves (recorded by git mv).
- Updated `Cargo.toml` for both crates.
- `moa-memory/src/lib.rs` reduced to ~80 lines of deprecated shims.
- All consumers compile.

## 7 Acceptance criteria

1. `cargo tree -p moa-memory --edges normal` shows no `pgvector`, `tiktoken-rs`, `reqwest` (unless transitively from a non-vector dep).
2. `cargo build --workspace` clean.
3. `cargo test --workspace` green.
4. `rg "moa_memory::vector|moa_memory::embedder|moa_memory::chunking" crates/ --type rust` returns ZERO hits — every consumer migrated or shim'd.
5. `cargo +nightly udeps -p moa-memory` clean.

## 8 Tests

```sh
cargo build --workspace
cargo test --workspace
cargo tree -p moa-memory
```

## 9 Cleanup

THIS IS the cleanup phase for vector code in `moa-memory`. Confirm:

- No `pgvector` import remains in `moa-memory/`.
- No vector-related test fixtures remain in `moa-memory/tests/fixtures/`.
- The deprecation shim has every former vector type pointing at the new crate, with a deprecation note.

## 10 What's next

**M14 — `moa-memory-ingest` crate scaffold (collect M10/M11/M12 logic into one crate).**

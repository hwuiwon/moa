# Step M14 — `moa-memory-ingest` crate scaffold

_Collect the slow-path VO (M10), fast-path API (M11), and contradiction detector (M12) into a dedicated crate. Move the IngestionVO out of `moa-orchestrator` so the orchestrator stays focused on session/turn lifecycle._

## 1 What this step is about

We landed M10/M11/M12 in `moa-orchestrator` and `moa-memory-ingest` (the latter scaffolded ad-hoc inside M12). This prompt creates the canonical `moa-memory-ingest` crate, moves the scattered ingest code into it, and re-exports just enough to let `moa-orchestrator` register the Restate VO. Crate boundary cleanliness sets up the M28 final-cleanup so that no `moa-memory-*` reference leaks into orchestrator concerns.

## 2 Files to read

- `crates/moa-orchestrator/src/ingestion_vo.rs` (M10)
- `crates/moa-orchestrator/src/fast_path.rs` (M11)
- `crates/moa-memory-ingest/src/contradiction.rs` (M12)
- `crates/moa-memory-ingest/prompts/judge.txt` (M12)

## 3 Goal

1. `moa-memory-ingest` crate with: `pub mod slow_path`, `pub mod fast_path`, `pub mod contradiction`, `pub mod chunking` (re-exported from `moa-memory-vector`), `pub mod connector` (M20 stub).
2. `IngestionVO` moves to `moa-memory-ingest::slow_path::IngestionVO`.
3. `moa-orchestrator` keeps only the **registration** of the VO with the Restate runtime; not the VO impl.
4. Fast-path public API at `moa_memory_ingest::fast_path::{fast_remember, fast_forget, fast_supersede}`.

## 4 Rules

- **No business logic in `moa-orchestrator`** beyond VO registration after this step.
- **Crate may depend on**: `moa-core`, `moa-runtime`, `moa-memory-graph`, `moa-memory-vector`, `moa-memory-pii`, restate-sdk, sqlx, anyhow, serde.
- **No dep on `moa-orchestrator` or `moa-session`** — keep dependency direction strict.
- **Tests live with the code** they test.

## 5 Tasks

### 5a Cargo.toml

```toml
[package]
name = "moa-memory-ingest"
version.workspace = true
edition.workspace = true

[dependencies]
async-trait = "0.1"
restate-sdk = "0.6"
sqlx = { workspace = true }
uuid = { workspace = true }
chrono = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = "1"
anyhow = "1"
tracing = "0.1"
tokio = { workspace = true, features = ["macros","rt"] }
moka = { version = "0.12", features = ["future"] }
blake3 = "1"
moa-core = { path = "../moa-core" }
moa-runtime = { path = "../moa-runtime" }
moa-memory-graph = { path = "../moa-memory-graph" }
moa-memory-vector = { path = "../moa-memory-vector" }
moa-memory-pii = { path = "../moa-memory-pii" }
```

### 5b Module layout

```
crates/moa-memory-ingest/
├── Cargo.toml
├── src/
│   ├── lib.rs
│   ├── slow_path.rs           ← from moa-orchestrator/src/ingestion_vo.rs
│   ├── fast_path.rs           ← from moa-orchestrator/src/fast_path.rs
│   ├── contradiction.rs       ← already here from M12
│   ├── connector.rs           ← stub for M20
│   ├── chunking.rs            ← thin re-export of moa-memory-vector::chunking
│   ├── extract.rs             ← LLM extractor; was inline in M10
│   ├── error.rs
│   └── ctx.rs                 ← shared `Ctx { graph, vector, pii, embedder, pool, ... }`
└── prompts/
    └── judge.txt
```

### 5c lib.rs

```rust
pub mod slow_path;
pub mod fast_path;
pub mod contradiction;
pub mod connector;
pub mod chunking;
pub mod extract;
pub mod ctx;
pub mod error;

pub use ctx::IngestCtx;
pub use error::IngestError;
pub use contradiction::{Conflict, ContradictionDetector, RrfPlusJudgeDetector};
pub use slow_path::IngestionVO;
pub use fast_path::{fast_remember, fast_forget, fast_supersede, FastRememberRequest, ForgetPattern};
```

### 5d Move from moa-orchestrator

```sh
git mv crates/moa-orchestrator/src/ingestion_vo.rs crates/moa-memory-ingest/src/slow_path.rs
git mv crates/moa-orchestrator/src/fast_path.rs    crates/moa-memory-ingest/src/fast_path.rs
```

### 5e moa-orchestrator becomes thin

`crates/moa-orchestrator/src/restate_register.rs`:

```rust
use moa_memory_ingest::IngestionVO;

pub fn register_with_restate(rt: &mut restate_sdk::HttpServer) {
    rt.bind(IngestionVO::DEFINITION);
}
```

`Cargo.toml`:

```toml
[dependencies]
moa-memory-ingest = { path = "../moa-memory-ingest" }
# (and remove direct deps on moa-memory-vector / moa-memory-graph if no longer needed here)
```

### 5f IngestCtx

`crates/moa-memory-ingest/src/ctx.rs`:

```rust
use std::sync::Arc;
use moa_memory_graph::GraphStore;
use moa_memory_vector::{VectorStore, Embedder};
use moa_memory_pii::PiiClassifier;

pub struct IngestCtx {
    pub graph:     Arc<dyn GraphStore>,
    pub vector:    Arc<dyn VectorStore>,
    pub embedder:  Arc<dyn Embedder>,
    pub pii:       Arc<dyn PiiClassifier>,
    pub contradict:Arc<dyn crate::contradiction::ContradictionDetector>,
    pub pool:      sqlx::PgPool,
}
```

Inject from `moa-runtime` startup. Replace globals from M10's prototype with this Ctx threaded through.

## 6 Deliverables

- New crate `moa-memory-ingest` with all sources + prompts.
- Updated `moa-orchestrator` (thin VO registration only).
- Workspace member updated.

## 7 Acceptance criteria

1. `cargo build --workspace` clean.
2. `rg "ingestion_vo|fast_path" crates/moa-orchestrator/` returns hits only in registration code.
3. End-to-end ingestion test still passes (M10 acceptance).
4. `cargo +nightly udeps -p moa-orchestrator` shows no leftover ingest-only deps.

## 8 Tests

```sh
cargo build --workspace
cargo test -p moa-memory-ingest
cargo test -p moa-orchestrator
```

## 9 Cleanup

- Delete the obsolete files from `moa-orchestrator/src/` (post-`git mv` they're already gone — confirm).
- Remove from `moa-orchestrator/Cargo.toml` any deps that are now only used by the moved code.

## 10 What's next

**M15 — Hybrid retriever (graph + vector + lexical, RRF k=60).**

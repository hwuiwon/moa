# STRUCT Pack — moa workspace structural refactor

**Target repo:** `hwuiwon/moa` at `main` (last verified commit `7f8d3689`)
**Goal:** Reorganize ~110k LOC of Rust source around explicit domain/use-case boundaries, isolate responsibilities, and break up files >1000 LOC. No behavior changes.
**Deliverable per prompt:** A self-contained Claude Code session that completes one structural change with `cargo check && cargo clippy --workspace --all-targets && cargo test --workspace --no-run` passing at the end.

---

## Locked-in decisions

These are constraints. Do not revisit them in any prompt.

| # | Decision | Rationale |
|---|---|---|
| 1 | **Keep `moa-` crate prefix** | Avoid `core` shadowing libcore; align with Rust workspace conventions (tokio-*, serde_*, bevy_*); reduce churn unrelated to architecture. |
| 2 | **DDD vocabulary, not full DDD ceremony** | Per-crate choice: two-layer (`core/` + `adapters/`) when the crate is "small core, many vendor adapters"; feature-folder when organized by capability. No three-layer domain/application/infrastructure split. |
| 3 | **No new crates except `moa-testing` and `moa-e2e`** (those are TEST pack territory) | Workspace already has 25 members; matklad's flat-workspace consensus says further splitting yields diminishing returns. |
| 4 | **Whole-workspace fair game for `pub` items** | Cross-crate import sites get rewritten in the same prompt that moves them. |
| 5 | **Embedding trait consolidation lives in `moa-core`** | `EmbeddingProvider` (currently `moa-providers`) and `Embedder` (currently `moa-memory-vector`) merge into one trait in `moa-core::traits`. |
| 6 | **`moa-orchestrator` bin/lib resolution: rename binary to `moa-orchestrator-bin`** | Less invasive than moving the library half into `moa-orchestrator-local`. |
| 7 | **Out of scope:** `services/` (Python sidecars), `skills/`, `examples/`, `dashboards/`, `ops/`, `k8s/`, `fly.toml`, `docker/`, `docs/` | Rust-only refactor. |

---

## Execution order

Prompts are sequenced. **Run in numeric order.** Each prompt lists its preconditions; do not skip ahead.

| Phase | Prompt | Crate / scope | Risk | Why now |
|---|---|---|---|---|
| **0. Workspace hygiene** | S01 | `Cargo.toml` (workspace root) | Low | Hoist common deps to `[workspace.dependencies]`; resolves coordination tax for downstream prompts. Reversible. |
| | S02 | All crates | Low | Add `cargo-hakari` workspace-hack. 20-25% build-time win during the refactor. Reversible. |
| **1. Foundation** | S03 | `moa-core` | Medium | Split `config.rs` (2,336 LOC). Stable foundation before downstream crates touch it. |
| | S04 | `moa-core` + `moa-providers` + `moa-memory-vector` | Medium | Consolidate embedding trait. Cross-crate; do once, propagate. |
| | S05 | `moa-orchestrator` (read-only audit) | Low | Confirm or remove the duplicate `pub trait` in `services/session_store.rs`. Read-only investigation prompt — outputs a decision document. |
| **2. Easy wins** | S06 | `moa-orchestrator-local` | Low | Split the 2,202-LOC `lib.rs` into modules. Pure mechanical cut along seams. |
| | S07 | `moa-session` | Low | Split `store.rs` (2,536 LOC). Single-crate change. |
| **3. Adapter-shaped crates** | S08 | `moa-providers` | Medium | Two-layer `core/` + `adapters/` split. Affects all consumers but is mostly module reorganization. |
| | S09 | `moa-hands` | Medium | Two-layer split + drop `moa-memory-ingest` runtime dependency. |
| **4. Capability-shaped crates** | S10 | `moa-brain` | High | Folder-by-folder splits of the 5 oversized pipeline/harness files. **Largest crate (~19.7k LOC); split into 5 sub-prompts S10a–S10e.** Also drop `moa-lineage-otel` direct dep. |
| | S11 | `moa-orchestrator` | Medium | Split objects (session, sub_agent), services (session_store), and rename binary to `moa-orchestrator-bin`. |
| | S12 | `moa-cli` | Medium | Split `main.rs` (3,132 LOC) and `commands/privacy.rs` (1,923 LOC). Standard clap-app-split pattern. |
| **5. Cleanup** | S13 | `moa-loadtest` | Low | Split `lib.rs` (2,195 LOC) and `scenarios/retrieval.rs` (1,303 LOC). |
| | S14 | Workspace-wide | Low | Final pass: `cargo machete` for unused deps, `cargo public-api` snapshot, dead-code sweep. |

**Total: 14 top-level prompts, with S10 expanding to 5 sub-prompts → 18 sessions total.**

Estimated single-developer wall-clock: 2–3 days if sessions run sequentially without surprises, 4–6 days realistically with rebase/CI churn.

---

## How each prompt is structured

Every `S##.md` follows the same shape so they're predictable:

1. **Scope** — one sentence
2. **Preconditions** — which prior prompts must be done; what should be green
3. **Files in scope** — explicit list, with current LOC
4. **Files explicitly out of scope** — guardrails
5. **Target structure** — the module tree after the prompt completes
6. **Step-by-step instructions** — ordered actions
7. **Verification** — exact commands that must pass
8. **Acceptance criteria** — what "done" looks like
9. **Rollback plan** — how to abort if something explodes
10. **Notes for the agent** — gotchas, anti-patterns, what *not* to refactor

---

## Conventions

- **No behavior changes.** This is a structural refactor. If a prompt finds a bug, it documents it in `REFACTOR_NOTES.md` for follow-up — does not fix it.
- **One prompt = one PR-sized change.** If a prompt's scope feels large mid-session, stop and split. Do not freelance.
- **`pub use` re-exports** are the bridge that keeps external call sites working during a split. Use them liberally; remove them in S14.
- **No new dependencies** added during this pack except `cargo-hakari` (S02). New deps are a separate decision.
- **Verification is not optional.** Every prompt ends with `cargo check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace --no-run`. The build must compile clean. Tests don't have to *run* (TEST pack handles that), but they must compile.

---

## Sequencing relative to TEST pack

**STRUCT pack runs first.** TEST pack assumes the structural reorganization is complete because:
- The kitchen-sink test files in S10/S11 territory will get split during STRUCT (file moves), and TEST pack splits them by *concern*. Doing TEST first means double work.
- The `moa-testing` shared crate (TEST pack) extracts helpers from files that STRUCT moves; if TEST runs first, helper extraction has to be redone.

If you must parallelize, the only safe overlap is: STRUCT phases 0–2 (S01–S07) can run before TEST starts; everything from S08 onward must complete before TEST T01.

---

## Files in this pack

```
struct-pack/
├── README.md                    ← this file
├── REFACTOR_NOTES.md            ← seed file; prompts append findings here
├── S01-workspace-deps-hoist.md
├── S02-cargo-hakari.md
├── S03-core-config-split.md
├── S04-embedding-trait-merge.md
├── S05-orchestrator-trait-audit.md
├── S06-orchestrator-local-split.md
├── S07-session-store-split.md
├── S08-providers-two-layer.md
├── S09-hands-two-layer.md
├── S10a-brain-pipeline-history.md
├── S10b-brain-pipeline-skills-and-query.md
├── S10c-brain-pipeline-compactor-and-mod.md
├── S10d-brain-harness-streaming.md
├── S10e-brain-misc-and-lineage-decoupling.md
├── S11-orchestrator-objects-and-bin-rename.md
├── S12-cli-split.md
├── S13-loadtest-split.md
└── S14-final-cleanup.md
```

---

## What this pack does NOT do

- Does not change any public API behavior, error type, or wire format.
- Does not introduce new traits beyond what's required for the embedding consolidation.
- Does not touch tests beyond making them compile (moves only when the file containing them moves).
- Does not push to the repo. Each prompt produces local commits; merging is human-driven.
- Does not run live tests or any test that hits external services.

When in doubt, do less. The point of this pack is to make the codebase *easier to change*, not to demonstrate cleverness.

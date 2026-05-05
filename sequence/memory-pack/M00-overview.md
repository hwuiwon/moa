# M00 — Migration overview (third revision)

_This doc is the canonical map for the post-M19 work. It supersedes the previous reshape-pack M00. Read this before running anything._

## Where we are

After M01–M19, the repo has both:

- **Legacy** file-wiki memory (`crates/moa-memory/`, `FileMemoryStore`, `MemoryStore` trait in `moa-core`) — still live, still wired into 32+ consumer sites.
- **New** graph stack (`moa-memory-graph`, `moa-memory-vector`, `moa-memory-pii`, `moa-memory-ingest`) — built and unit-tested but not yet the only memory subsystem.

The work below cuts the legacy system out and finishes the graph stack's privacy/audit story.

## Phase plan

| Phase | Prompt | Purpose | Status |
|-------|--------|---------|--------|
| Structure | **S01** | Move every crate into `crates/` | Pending |
| Cutover | **C00** | Read-only orientation | Pending |
| Cutover | **C01** | Inventory all `moa_memory::*` consumers | Pending |
| Cutover | **C02** | Reshape `moa-cli` memory commands | Pending |
| Cutover | **C03** | Migrate orchestrators | Pending |
| Cutover | **C04** | Migrate skills + tail consumers | Pending |
| Cutover | **C05** | Delete `MemoryStore` trait + wiki types from `moa-core` | Pending |
| Cutover | **C06** | Delete `moa-memory` crate | Pending |
| Reshape | **R01** | Group `moa-memory-*` under `crates/moa-memory/` | Pending |
| Reshape | **R02** | Type-location audit + delete connector stubs | Pending |
| ~~Connectors~~ | ~~**M20**~~ | ~~Connector trait~~ | **DELETED** — connectors deferred indefinitely |
| Privacy | **M21** | Envelope encryption — **decision-log only** | Replaces full implementation |
| Audit | **M22** | pgaudit + S3 Object Lock | Unchanged from M-pack |
| Privacy | **M23** | Privacy export CLI | Simplified — no decryption step |
| Privacy | **M24** | Privacy erase CLI | Simplified — hard-purge only, no crypto-shred |
| Privacy | **M25** | Cross-tenant pentest | 11 attacks; redaction-bypass replaces KEK substitution as 5i |
| Audit | **M26** | Privacy audit GitHub Action | Unchanged |
| Privacy | **M27** | DSAR runbook | Unchanged |
| Cleanup | **M28** | Final cleanup + CI guardrails | Slimmed (C06 absorbed crate deletion) |
| Privacy | **M29** | Privacy SLA dashboards | Unchanged |
| Cleanup | **M30** | Architecture doc rewrite | Unchanged |

## Architectural decisions (locked)

These are baked into the prompts above. Future contributors should not relitigate without RFC.

1. **Full cutover from file-wiki to graph.** Both systems were live as of M19; that ended with C06.
2. **`crates/` workspace layout.** Production convention; locked in S01.
3. **`moa-memory/` folder grouping** for the four subcrates. Crate package names unchanged.
4. **Connectors deferred indefinitely.** The connector track was originally M20; deleted from the plan. M14 stubs removed in R02. Future connector work, if any, lives in a new `moa-connectors/` crate gated behind an explicit RFC.
5. **Envelope encryption deferred (Path A: redaction at ingestion is the privacy boundary).** M21 documents the decision; M22's pgaudit + Object Lock provide tamper-evidence; M24 is hard-purge only. If a future regulatory ask requires per-fact crypto-shred, design a Path B addendum at that time.
6. **`moa-orchestrator-local`** stays the embedded runtime. Optional rename to `moa-orchestrator-embedded` in R02 — left to project judgment.

## Crate structure — end state (after R01)

```
moa/
├── Cargo.toml
├── architecture.md
├── docs/
│   ├── architecture/
│   │   └── type-placement.md       ← R02
│   └── migrations/
│       └── moa-memory-inventory.md ← C01
└── crates/
    ├── moa-core/
    ├── moa-brain/
    ├── moa-cli/
    ├── moa-desktop/
    ├── moa-eval/
    ├── moa-gateway/
    ├── moa-hands/
    ├── moa-loadtest/
    ├── moa-memory/                 ← grouping parent
    │   ├── README.md
    │   ├── graph/                  ← moa-memory-graph crate
    │   ├── vector/                 ← moa-memory-vector crate
    │   ├── pii/                    ← moa-memory-pii crate
    │   └── ingest/                 ← moa-memory-ingest crate
    ├── moa-orchestrator/
    ├── moa-orchestrator-local/     ← (or moa-orchestrator-embedded if renamed)
    ├── moa-providers/
    ├── moa-runtime/
    ├── moa-security/
    ├── moa-session/
    └── moa-skills/
```

## Old → replacement deletion ledger

| Deleted | Replaced by | Where |
|---------|-------------|-------|
| `moa-memory` crate | `moa-memory/{graph,vector,pii,ingest}` (grouped under `moa-memory/`) | C06, R01 |
| `MemoryStore` trait (`moa-core`) | `GraphStore` trait + `RetrievalHandle` / `IngestHandle` traits | C05 |
| `WikiPage`, `MemoryPath`, `PageType`, `IngestReport`, `MemorySearchMode`, `MemorySearchResult`, `PageSummary`, `ConfidenceLevel` (`moa-core`) | (no replacement; graph types replace these semantically) | C05 |
| `FileMemoryStore` | `AgeGraphStore` | C03 |
| `MemoryStore::search` call sites | `HybridRetriever::retrieve` | C02–C04 |
| `MemoryStore::ingest_source` call sites | `IngestionVO::ingest_turn` (slow) or `fast_remember` (fast) | C02–C04 |
| `MemoryStore::write_page` call sites | `fast_remember` or `IngestionVO::ingest_turn` | C03 |
| `MemoryStore::read_page` / `list_pages` / `get_index` call sites | `GraphStore::get_node` / `lookup_seeds` / retriever, or retired with explicit decision | C01 decisions, applied in C02–C04 |
| `MemoryStore::rebuild_search_index` | (retired; graph indexes are write-incremental) | C02 |
| `moa memory show <path>` CLI | `moa memory show <uid>` (or retired per C01 decision) | C02 |
| `moa memory rebuild-index`, `moa memory rebuild-embeddings` CLI | (retired) | C02 |
| Connector stubs in `moa-memory/ingest/` (M14 era) | (deleted; connectors deferred) | R02 |
| `M20-connector-trait.md` prompt | (deleted from pack) | M00 (this doc) |
| Original `M21-envelope-encryption.md` | `M21-envelope-encryption-deferred.md` (decision-log only) | M-pack v2 |
| Original `M28-delete-moa-memory.md` | `M28-final-cleanup.md` (slim; C06 absorbed crate deletion) | M-pack v2 |

## Recommended running order

```
S01            ← restructure to crates/ layout
C00 (read)     ← cutover orientation
C01            ← inventory; STOP and review docs/migrations/moa-memory-inventory.md
C02 → C06      ← cutover phases in order; build green between each
R01 → R02      ← grouping + audit polish
M21 (read)     ← envelope encryption decision-log
M22 → M30      ← finish privacy + audit + cleanup
```

## When to break sequence

- **You can stop after C06** and have a working production-ready cutover. R01–R02 are organizational polish; M21+ is privacy/audit work that adds value but is independent of the graph stack itself.
- **You can run M21+ in parallel** with R01–R02 (different files, no overlap).
- **You cannot reorder C-prompts.** C02→C06 is a strict topological sort: each one's preconditions are satisfied by its predecessor.

## Sanity checks at each phase boundary

After every prompt completes, run the same three checks:

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

Plus the prompt-specific acceptance criteria. If any fail, stop and resolve before continuing.

## Where to find historical context

- **C01's inventory** documents every consumer migration decision: `docs/migrations/moa-memory-inventory.md`.
- **C00** explains the semantic gap between the wiki and graph systems.
- **M21 decision-log** documents the Path A choice on encryption.
- **R02's type-placement doc** locks in where types live for future contributors.

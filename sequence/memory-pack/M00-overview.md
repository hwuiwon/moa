# M00 — MOA Graph-Primary Memory: Prompt Pack Overview

_Read this before running any other prompt in the M-series._

## What this pack does

This is the implementation prompt pack for replacing MOA's file-wiki memory system with a **graph-primary, bi-temporal memory architecture** built on Apache AGE (Postgres extension), pgvector, Turbopuffer (cloud opt-in), Cohere Embed v4, and Restate orchestration. It also adds the **3-tier scope prerequisite** (`MemoryScope::Global`) that the graph design hard-depends on.

The pack is **31 prompts (M00–M30)**, each self-contained and feedable to an LLM coding agent one at a time. Cleanup is **interleaved** — every prompt that introduces a replacement deletes what it replaces in the same step. There is no consolidated cleanup phase except the final crate-deletion sweep in M28.

**Out of scope for this pack** (sequenced separately):
- Tool/skill overlay system (parallel track, separate M-pack to be created if needed).
- Real platform connector implementations (Slack/Notion/Drive). M20 builds the abstract `Connector` trait + `MockConnector` only.
- Multimodal embeddings (text-only Cohere v4 for v1; multimodal upgrade path preserved in schema via `embedding_model_version`).

## How to use these prompts

1. **Run them in order.** M01 depends on nothing prior; every later prompt depends on its predecessors. Skipping creates compile failures.
2. **Feed one prompt at a time** to an LLM coding agent (Claude, GPT, etc.). Each prompt names every file the agent should read first, the exact tasks, acceptance criteria, and tests.
3. **Verify acceptance criteria before moving on.** Each prompt has an "Acceptance criteria" section with concrete pass/fail signals. If the agent skips one, run the prompt again with explicit instruction to satisfy it.
4. **Run the cleanup section.** Every prompt has a "Cleanup" section that deletes specific files/code. Cleanup is non-optional — it prevents dead code from accumulating.

## SOTA-pinned technology stack

These versions were verified SOTA-credible as of late April 2026. Pin them in `Cargo.toml` and `docker-compose.yml`.

| Component | Version | Notes |
|---|---|---|
| PostgreSQL | **17.6+** (mandatory) | 17.5 has a logical-decoding bug that breaks Debezium |
| Apache AGE | `release/PG17/1.7.0` | First-class RLS via PR #2309 |
| pgvector | **0.8.2** | halfvec; 0.8.0 lacks iterative-scan and PG18 build fixes |
| pgaudit | **17.x branch** | PG17 ABI |
| Restate | **1.6.1+** | Feb 2026 patch fixes RocksDB rebalancing |
| Debezium PostgreSQL connector | **V2 (3.5.x)** | V1 EOL March 2026 |
| Turbopuffer | current | SOC2 Type 2, HIPAA BAA on Scale/Enterprise; BYOC GA |
| Cohere Embed v4 | current | 1024-dim Matryoshka |
| Cohere Rerank | **v4.0-fast** (interactive), **v4.0** (offline) | NOT v3; v4 released Dec 2025 |
| openai/privacy-filter | HuggingFace `openai/privacy-filter` | Apache 2.0; ingestion-time PII classifier |
| Rust UUID | `uuid` crate v1.x with `v7` feature | PG18 has native `uuidv7()` for future migration |

## Old-component → replacement mapping

This is the deletion ledger. Every entry is removed by the listed M-prompt. By the end of M28 nothing in the left column should remain in the codebase.

| Old (deleted) | Replacement | Where deleted |
|---|---|---|
| `moa-memory` crate | `moa-memory-graph` + `moa-memory-vector` + `moa-memory-pii` + `moa-memory-ingest` | M28 |
| `FileMemoryStore` impl | `GraphStore` trait (AGE adapter) | M07 |
| `MemoryStore` trait | Split into `GraphStore` + `VectorStore` + `LexicalStore` | M07/M13 |
| `MEMORY.md` handler | Graph nodes (Entity/Concept/Decision/Incident/Lesson/Fact) | M03/M07 |
| `_log.md` handler | `moa.graph_changelog` table + Debezium feed | M06 |
| Wiki branching/reconciliation | Bi-temporal SUPERSEDES edges + valid_time_end | M08 |
| Page-level consolidation cron | Aging policy + LLM contradiction detector | M12 |
| FileWiki tsvector index | `moa.node_index` sidecar + Postgres tsvector on properties | M04 |
| On-disk skill loader (`skills/*.md`) | `moa.skill` Postgres rows | M18 |
| Pre-3-tier `MemoryScope` enum | `MemoryScope::{Global, Workspace(_), User(_)}` | M01 |
| Pre-3-tier RLS policies | 3-tier-aware RLS FORCE policies | M02 |
| Cohere Rerank v3 | Cohere Rerank v4.0-fast / v4.0 | M15 |
| (no PII classification) | `openai/privacy-filter` via `moa-memory-pii` | M09 |
| (no atomic write protocol) | M08 single-tx write across graph+sidecar+vector+changelog | M08 |
| (no audit immutability) | M22 pgaudit + S3 Object Lock 6yr | M22 |

## Crate structure after the pack lands

**Existing crates** (touched but not deleted):
- `moa-core` (M01: scope enum)
- `moa-brain` (M15/M16: hybrid retriever + planner)
- `moa-cli` (M23/M24/M27: privacy + admin commands)
- `moa-eval` (M25/M29: pen-test + golden e2e)
- `moa-loadtest` (M30: perf gate)
- `moa-orchestrator` (M10/M11: ingestion VOs)
- `moa-runtime` (M02/M21: GUC discipline + envelope wiring)
- `moa-security` (M21: KEK/DEK envelope)
- `moa-skills` (M18/M19: Postgres-backed + cross-references)

**New crates** (introduced by this pack):
- `moa-memory-graph` — AGE adapter, Cypher templates, bi-temporal write protocol
- `moa-memory-vector` — `VectorStore` trait, pgvector + Turbopuffer impls
- `moa-memory-pii` — openai/privacy-filter helper (reusable across components)
- `moa-memory-ingest` — slow/fast path ingestion, contradiction detector

**Deleted crate**:
- `moa-memory` (final removal in M28)

## Phase plan at a glance

| # | Title | Crate(s) touched |
|---|---|---|
| M01 | 3-tier `MemoryScope::Global` enum | moa-core |
| M02 | 3-tier RLS + GUC discipline | moa-runtime, migrations |
| M03 | Apache AGE bootstrap migration | migrations |
| M04 | `moa.node_index` sidecar | migrations, moa-memory-graph |
| M05 | `VectorStore` trait + pgvector impl | moa-memory-vector |
| M06 | `graph_changelog` outbox + Debezium config | migrations, moa-memory-graph |
| M07 | `moa-memory-graph` crate scaffold | moa-memory-graph |
| M08 | Bi-temporal write protocol (atomic tx) | moa-memory-graph |
| M09 | `moa-memory-pii` crate (privacy-filter) | moa-memory-pii |
| M10 | Slow-path ingestion VO (Restate) | moa-memory-ingest, moa-orchestrator |
| M11 | Fast-path ingestion (<500ms) | moa-memory-ingest |
| M12 | Contradiction detector (RRF + LLM judge) | moa-memory-ingest |
| M13 | Split vector code out of moa-memory | moa-memory-vector |
| M14 | `moa-memory-ingest` crate scaffold | moa-memory-ingest |
| M15 | Hybrid retriever (graph+vector+lexical, RRF k=60) | moa-brain |
| M16 | Query planner (NER, scope, layer-bias) | moa-brain |
| M17 | Read-time cache (changelog-version invalidation) | moa-brain, migrations |
| M18 | Skills → Postgres rows | moa-skills |
| M19 | Skill ↔ graph cross-references | moa-skills, moa-memory-graph |
| M20 | `Connector` trait + `MockConnector` | moa-memory-ingest |
| M21 | KEK/DEK envelope encryption | moa-security |
| M22 | pgaudit + S3 Object Lock shipping | migrations, ops |
| M23 | `moa privacy export` CLI | moa-cli |
| M24 | `moa privacy erase` CLI (crypto-shred default) | moa-cli |
| M25 | Cross-tenant pen-test suite | moa-eval |
| M26 | Turbopuffer `VectorStore` impl | moa-memory-vector |
| M27 | Workspace promotion (pgvector → Turbopuffer, dual-read) | moa-cli, moa-memory-vector |
| M28 | DELETE `moa-memory` crate; final wiki sweep | (deletion) |
| M29 | Validation: 100-fact golden e2e | moa-eval |
| M30 | Performance gate (P95 ≤ 80ms @ 100 QPS) | moa-loadtest |

## Conventions every prompt follows

```
# Step MXX — title
_one-line italicized goal_

## 1 What this step is about    -- context, why
## 2 Files to read              -- explicit file paths the agent must read first
## 3 Goal                       -- what the deliverable looks like
## 4 Rules                      -- non-negotiable constraints
## 5 Tasks                      -- step-by-step (5a, 5b, ...)
## 6 Deliverables               -- file list with line-count guidance
## 7 Acceptance criteria        -- numbered, testable
## 8 Tests                      -- how to verify
## 9 Cleanup                    -- what to delete in this step
## 10 What's next               -- pointer to next M-prompt
```

## What's next

Run **M01** — Add `MemoryScope::Global` to `moa-core`. This is the only prerequisite the graph work depends on, and it's the smallest mechanical change in the pack.

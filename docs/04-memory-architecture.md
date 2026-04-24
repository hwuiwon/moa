# 04 — Memory Architecture

_File-backed wiki, Postgres keyword search, pgvector semantic search, and consolidation._

## Principles

1. Markdown files are canonical.
2. Postgres indexes are derived and rebuildable.
3. User and workspace scopes compose at runtime.
4. Memory updates should be inspectable, attributable, and reversible through normal file and event history.
5. Memory is part of the learning pipeline, not a separate cache.

## Scopes

| Scope | Local path | Cloud path | Contents |
|---|---|---|---|
| User | `~/.moa/memory/` | `users/{user_id}/memory/` | preferences, habits, corrections, cross-workspace facts |
| Workspace | `~/.moa/workspaces/{workspace_id}/memory/` | `workspaces/{workspace_id}/memory/` | project architecture, conventions, decisions, sources, skills |

At context compilation time the memory stage loads both scope indexes, then retrieves task-relevant pages from user and workspace search.

## Wiki Layout

```text
memory/
  MEMORY.md
  _schema.md
  _log.md
  topics/
  entities/
  decisions/
  skills/
  sources/
```

`MEMORY.md` is the compact index. Topic/entity/decision/source/skill pages carry frontmatter for page type, timestamps, confidence, tags, related pages, source provenance, and reference counters.

## Postgres Search Index

`moa-memory` maintains a Postgres `wiki_pages` table derived from the markdown files:

- weighted `search_tsv` generated column for keyword search
- trigram index for title fallback and typo-prone short queries
- `embedding vector(1536)` for semantic search
- `wiki_embedding_queue` for asynchronous embedding work

Retrieval modes:

| Mode | Behavior |
|---|---|
| `keyword` | Uses weighted `tsvector` search and trigram fallback |
| `semantic` | Embeds the query and searches page embeddings with pgvector cosine distance |
| `hybrid` | Runs keyword and semantic retrieval and fuses rankings with reciprocal rank fusion |

Semantic search is eventually consistent. Page writes update keyword search immediately and enqueue embedding refresh work when an embedding provider is configured.

## Context Pipeline Integration

The memory processor currently runs after query rewriting and before history compilation. It uses the rewritten query when available, otherwise it extracts keywords from the latest user message.

It inserts:

- truncated user `MEMORY.md`
- truncated workspace `MEMORY.md`
- top relevant pages from user and workspace scopes

Memory content is inserted as a user-role reminder near the active turn so static prompt prefix caching remains stable.

## Writes And Ingestion

Memory can change through:

- explicit `memory_write` tool calls
- source ingestion
- skill distillation and improvement
- consolidation
- manual user edits to markdown files

After writes, the file store updates the derived Postgres index and, when semantic search is enabled, the embedding queue.

## Consolidation

Workspace consolidation is a scheduled maintenance pass. In cloud mode it is the `Consolidate` Restate workflow. Locally it runs through the local maintenance path.

Consolidation can:

- normalize relative dates
- resolve contradictions
- prune stale facts
- merge duplicates
- flag orphaned pages
- decay confidence
- regenerate `MEMORY.md`

Successful consolidation appends a `memory_updated` entry to `learning_log`.

## Learning Relationship

Memory is one output of the broader learning loop:

```text
Task segments
  -> resolution scores
  -> learning_log
  -> skill ranking, intent discovery, memory consolidation
```

Memory pages explain what MOA knows; `learning_log` explains how and when a learned update entered the system.

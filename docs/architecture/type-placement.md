# Type Placement Policy

This document records where shared Rust types live in the MOA workspace. A type
has one canonical definition site; other crates import that type instead of
defining a lookalike.

## `moa-core`

`moa-core` owns tenant primitives, session/event DTOs, shared errors, and trait
surfaces. It does not depend on the memory subsystem.

- IDs: `WorkspaceId`, `UserId`, `SessionId`
- Scopes: `MemoryScope`, `ScopeTier`, `ScopeContext`
- Config and errors: `MoaConfig`, `MoaError`, `Result`
- Session and event DTOs: `SessionMeta`, `SessionStatus`, `SessionFilter`,
  `SessionSummary`, `Event`, `EventRecord`, `EventStream`, `EventRange`,
  `EventFilter`
- Trait surfaces: `BrainOrchestrator`, `SessionStore`, `BlobStore`,
  `BranchManager`, `HandProvider`, `LLMProvider`, `PlatformAdapter`,
  `BuiltInTool`, `ContextProcessor`, `CredentialVault`
- Tool execution context: `ToolContext`

## `moa-memory/graph`

`moa-memory/graph` owns graph-primary memory storage and graph-shaped data.

- `GraphStore`, `AgeGraphStore`
- `NodeLabel`, `EdgeLabel`
- `NodeIndexRow`, `NodeWriteIntent`, `EdgeWriteIntent`
- `PiiClass`
- `ChangelogRecord`, `LexicalStore`
- `GraphError`

`PiiClass` lives here because it is persisted on graph nodes and used for
retrieval filtering. The classifier implementation lives in `moa-memory/pii`
and returns this canonical enum.

## `moa-memory/vector`

`moa-memory/vector` owns embedding and vector-index abstractions.

- `Embedder`, `CohereV4Embedder`
- `VectorStore`, `PgvectorStore`, `TurbopufferStore`
- Vector query/result DTOs such as `VectorQuery` and `VectorMatch`

## `moa-memory/pii`

`moa-memory/pii` owns privacy classification and redaction clients.

- `PiiClassifier`
- `PiiResult`, `PiiSpan`, `PiiCategory`
- `OpenAiPrivacyFilterClassifier`
- `MockClassifier` test helper

This crate re-exports `moa_memory_graph::PiiClass` for classifier callers, but
does not define its own privacy class enum.

## `moa-memory/ingest`

`moa-memory/ingest` owns ingestion DTOs and write-path orchestration.

- `IngestionVO`, `IngestionVOImpl`, `IngestionVOClient`
- `SessionTurn`
- `IngestApplyReport`, `IngestDecision`
- `fast_remember`, `fast_forget`, `fast_supersede`
- `ContradictionDetector`, `RrfPlusJudgeDetector`

Connector stubs are intentionally absent. External connectors are deferred and
should not be reintroduced as placeholders.

## `moa-brain`

`moa-brain` owns retrieval and context compilation.

- `HybridRetriever`
- Query planning and retrieval request/result DTOs
- Context processors and pipeline stages
- Reranker public surface used by retrieval

## Where New Types Go

| Type kind | Crate |
|---|---|
| ID newtype shared by two or more crates | `moa-core` |
| Trait surface shared by two or more crates | `moa-core` |
| Implementation of a `moa-core` trait | Implementing crate |
| Graph node, edge, or sidecar type | `moa-memory/graph` |
| Embedding or vector-index type | `moa-memory/vector` |
| Privacy classifier type | `moa-memory/pii` |
| Ingestion pipeline DTO | `moa-memory/ingest` |
| Retrieval or context pipeline type | `moa-brain` |

## Anti-Patterns

- Defining the same public type in two crates.
- Putting graph-specific types in `moa-core` because another crate might need
  them later.
- Adding compatibility aliases for the deleted file-wiki memory system.
- Adding empty connector traits or clients before connector work is actively
  scheduled.

History: this policy was finalized after the C-pack memory cutover and R01
folder grouping. See `docs/migrations/moa-memory-inventory.md` for the migration
record.

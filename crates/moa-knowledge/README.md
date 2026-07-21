# moa-knowledge

Tenant knowledge base for MOA. Ingests documents from linked-account
providers (e.g. Nango-connected sources) through parsing, chunking, and
semantic graph extraction into graph and vector writes, with Postgres
repositories for connections, documents, and sync state.

## Structure

- `domain` — domain types for tenant knowledge connections, parsing, blocks,
  and chunks.
- `providers` — linked-account provider traits and adapters (Nango, merge).
- `parser` — document parser trait and adapters (native, LlamaParse,
  Reducto, Unstructured).
- `chunking` — deterministic block and chunk construction.
- `normalize` — provider-record and text normalization helpers.
- `semantic_graph` — schema-constrained semantic graph extraction for
  knowledge chunks.
- `semantic_graph_model` — model-backed semantic graph extractor used in
  production when a provider is configured.
- `graph_delta` — graph delta types emitted by knowledge ingestion.
- `ingestion` — pipeline from provider records to graph/vector writes.
- `repository` — repository traits and Postgres implementations for tenant
  knowledge persistence.
- `contact_groups` — contact-group derivation seams for knowledge evidence.
- `observability` — redacted observability helpers for knowledge ingestion.
- `error` — crate `Error`/`Result` types.

## Relationship To Graph Memory

Builds on `moa-memory-graph` and `moa-memory-types` to write extracted
knowledge into the same relational graph store used by conversational
memory. Driven at runtime by `moa-orchestrator`.

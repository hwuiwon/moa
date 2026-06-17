# moa-lineage

Two-tier observability and explainability for MOA. The subcrates here form one
logical unit; they are separated to keep the hot path (`sink`), the wire format
(`otel`), and the record shapes (`core`) independently versionable. `audit/`
owns the opt-in compliance tier.

## Subcrates

| Path | Crate name | Responsibility |
| --- | --- | --- |
| `core/` | `moa-lineage-core` | `LineageSink` trait; record shapes; scope and ID types; serde wire format |
| `citation/` | `moa-lineage-citation` | Provider citation adapters plus BM25/NLI answer-source verification |
| `sink/` | `moa-lineage-sink` | mpsc + fjall durable journal + TimescaleDB writer + worker lifecycle |
| `otel/` | `moa-lineage-otel` | OTel GenAI v1.38 + OpenInference attribute emitters; tracing bridge |
| `audit/` | `moa-lineage-audit` | BLAKE3 hash chain + ct-merkle + Object Lock + PII HMAC vault |

## Public surface

- `moa_lineage_core::{LineageSink, LineageEvent, RetrievalLineage, ContextLineage, GenerationLineage, TurnId}`
- `moa_lineage_citation::{CitationAdapter, CascadeVerifier, ChunkRef}`
- `moa_lineage_sink::{MpscSink, NullSink, MpscSinkConfig, WriterHandle}`
- `moa_lineage_otel::{emit_retrieval_attrs, emit_generation_attrs, emit_context_attrs}`

Lineage explain, retrieval debug, query, and export operations are exposed
through hosted orchestrator/edge APIs, not a local command client crate.

Database schema for lineage lives in `crates/moa-migrations`. Production gets
the tables through the central refinery baseline; the lineage writer uses the
central `sql/lineage_schema.sql` fragment for standalone schema bootstrap.

## Phase status

L01 shipped core + sink + otel; L03 wired eval and dashboards; L04 ships the
compliance audit tier behind a per-workspace opt-in. Current architecture
details live in `docs/01-architecture-overview.md` and
`docs/10-technology-stack.md`.

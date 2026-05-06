# moa-lineage

Two-tier observability and explainability for MOA. The subcrates here form one
logical unit; they are separated to keep the hot path (`sink`), the wire format
(`otel`), the record shapes (`core`), citation verification (`citation`), and
cold retention (`cold`) independently versionable. `audit/` will be added by L04.

## Subcrates

| Path | Crate name | Responsibility |
| --- | --- | --- |
| `core/` | `moa-lineage-core` | `LineageSink` trait; record shapes; scope and ID types; serde wire format |
| `sink/` | `moa-lineage-sink` | mpsc + fjall durable journal + TimescaleDB writer + worker lifecycle |
| `otel/` | `moa-lineage-otel` | OTel GenAI v1.38 + OpenInference attribute emitters; tracing bridge |
| `citation/` | `moa-lineage-citation` | Vendor passthrough adapters + cascade verifier |
| `cold/` | `moa-lineage-cold` | Parquet/object-store exporter + retention progress tracking |
| `audit/` | `moa-lineage-audit` | (L04) BLAKE3 hash chain + ct-merkle + Object Lock + PII HMAC vault |

## Public surface

- `moa_lineage_core::{LineageSink, LineageEvent, RetrievalLineage, ContextLineage, GenerationLineage, TurnId}`
- `moa_lineage_sink::{MpscSink, NullSink, MpscSinkConfig, WriterHandle}`
- `moa_lineage_otel::{emit_retrieval_attrs, emit_generation_attrs, emit_context_attrs}`

The CLI subcommands (`moa explain`, `moa retrieve --debug`, `moa lineage query`,
`moa lineage export`) live in `crates/moa-cli/`.

Database schema for lineage lives in the central Postgres migration tree at
`crates/moa-session/migrations/postgres/024_lineage.sql`; lineage crates do not
own separate migration directories.

## Phase status

L01 shipped core + sink + otel; L02 ships citation + cold; L03 wires eval and
dashboards; L04 adds compliance audit. See `sequence/lineage/L*-*.md` for
prompts.

# moa-lineage

Two-tier observability and explainability for MOA. The subcrates here form one
logical unit; they are separated to keep the hot path (`sink`), the wire format
(`otel`), and the record shapes (`core`) independently versionable. `citation/`,
`cold/`, and `audit/` will be added by L02-L04.

## Subcrates

| Path | Crate name | Responsibility |
| --- | --- | --- |
| `core/` | `moa-lineage-core` | `LineageSink` trait; record shapes; scope and ID types; serde wire format |
| `sink/` | `moa-lineage-sink` | mpsc + fjall durable journal + TimescaleDB writer + worker lifecycle |
| `otel/` | `moa-lineage-otel` | OTel GenAI v1.38 + OpenInference attribute emitters; tracing bridge |
| `citation/` | `moa-lineage-citation` | (L02) vendor passthrough adapters + cascade NLI verifier |
| `cold/` | `moa-lineage-cold` | (L02) Parquet + S3 exporter; retention policy |
| `audit/` | `moa-lineage-audit` | (L04) BLAKE3 hash chain + ct-merkle + Object Lock + PII HMAC vault |

## Public surface

- `moa_lineage_core::{LineageSink, LineageEvent, RetrievalLineage, ContextLineage, GenerationLineage, TurnId}`
- `moa_lineage_sink::{MpscSink, NullSink, MpscSinkConfig, WriterHandle}`
- `moa_lineage_otel::{emit_retrieval_attrs, emit_generation_attrs, emit_context_attrs}`

The CLI subcommands (`moa explain`, `moa retrieve --debug`, `moa lineage query`,
`moa lineage export`) live in `crates/moa-cli/`.

## Phase status

L01 ships core + sink + otel; L02 adds citation + cold; L03 wires eval and
dashboards; L04 adds compliance audit. See `sequence/lineage/L*-*.md` for
prompts.

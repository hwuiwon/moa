# Lineage pack — observability and explainability for MOA

This pack adds two-tier explainability to MOA:

- **Engineering tier** (always-on): retrieval lineage, context lineage, generation lineage, post-hoc citations. Powers `moa explain <session>`, `moa retrieve --debug`, Grafana dashboards, eval-regression alerts.
- **Compliance tier** (opt-in per workspace): adds BLAKE3 hash chain, RFC 6962 Merkle roots via `ct-merkle`, S3 Object Lock retention, PII pseudonymization vault, DSAR export. Maps to EU AI Act Article 12 / GDPR Article 22 / NIST AI RMF / ISO 42001.

## Crate layout (folder grouping under `crates/moa-lineage/`)

```
crates/moa-lineage/
├── README.md
├── core/        ← package: moa-lineage-core      (traits, data model, scope types)
├── sink/        ← package: moa-lineage-sink      (mpsc + fjall + COPY → TimescaleDB)
├── otel/        ← package: moa-lineage-otel      (OTel + OpenInference attribute emitters)
├── citation/    ← package: moa-lineage-citation  (vendor passthrough adapters + cascade verifier)
├── cold/        ← package: moa-lineage-cold      (Parquet + S3 exporter, retention)
└── audit/       ← package: moa-lineage-audit     (BLAKE3 chain + Merkle + Object Lock + PII vault)
```

CLI subcommands (`moa explain`, `moa retrieve --debug`, `moa lineage query`, `moa lineage export`) live in the existing `crates/moa-cli/`. Same pattern as memory.

## Design decisions baked in

1. **TimescaleDB extension on existing Postgres 17.** Hypertable + 7-day compression policy + 30-day retention with rollover to S3 Parquet cold tier. ~$10/mo at 1M turns/day vs ~$248/mo plain jsonb.
2. **Async dual-write.** Hot path is one bounded `tokio::sync::mpsc::try_send` — never blocks turn latency. Background worker drains via fjall durable journal → COPY into TimescaleDB → 30-second/50 MB Parquet rolls to S3.
3. **At-least-once.** Idempotency keys on `(turn_id, record_kind, ts)` absorb duplicates. Hard-kill loses anything that hasn't reached fjall — bounded by an 8 K mpsc channel and per-MB fjall flush.
4. **OTel GenAI semconv v1.38** as wire format, dual-emit OpenInference for Phoenix interop. No deprecated `gen_ai.prompt`/`gen_ai.completion`.
5. **Vendor citations + cascade verifier.** Adapters for Anthropic, OpenAI, Cohere, Vertex. NLI cascade backstop catches citation hallucination. Always-on; cheap.
6. **Per-workspace compliance opt-in.** Engineering tier writes to `analytics.turn_lineage` always. Compliance tier adds `prev_hash` + Merkle roots when the workspace flag is set.
7. **Folder grouping over crate flattening.** Six subcrates under one parent, sibling pattern to `crates/moa-memory/`.

## Files in this pack

| # | File | Adds | Phase |
|---|---|---|---|
| L01 | `L01-lineage-scaffold-core-sink-otel.md` | core/ + sink/ + otel/ subcrates; `LineageSink` trait wired into `ToolContext`; orchestrator emits real events; `moa explain <session>` and `moa retrieve --debug` | Phase 1 (engineering tier — design floor) |
| L02 | `L02-citation-cold-tier-gemini-embedder.md` | citation/ + cold/ subcrates; vendor passthrough for Anthropic/OpenAI/Cohere/Vertex; cascade NLI verifier; Gemini Embedding 001 added to moa-memory/vector/; `moa lineage query` | Phase 2 (production observability) |
| L03 | `L03-eval-grafana-alerts.md` | bridge from moa-eval to lineage; ScoreRecord; continuous-aggregate panels; zero-recall and grounding-regression alerts | Phase 2 (parallel track with L02) |
| L04 | `L04-compliance-audit-merkle-objectlock.md` | audit/ subcrate; BLAKE3 hash chain; ct-merkle Merkle root publishing; S3 Object Lock; PII HMAC vault; `moa lineage export` for DSAR; pgaudit integration | Phase 3 (compliance tier) |

## Recommended running order

```
L01           ← scaffold + engineering tier
              ←   build green; one orchestrator emitting; "moa explain" works end-to-end
              ←   STOP and validate schema before adding cost
L02           ← citation + cold tier + Gemini embedder
              ←   can fan out: L02 main, L02 has a sub-step for the embedder
L03           ← eval + dashboards (CAN RUN PARALLEL WITH L02)
L04           ← compliance tier
              ←   includes external crypto review checkpoint before claiming compliance
```

## Estimated effort

- L01: 3–4 engineer-weeks
- L02: 5–6 engineer-weeks
- L03: 2–3 engineer-weeks (parallel)
- L04: 6–8 engineer-weeks plus external cryptographic review

## What's NOT in scope (intentionally)

- **End-user UI** — data layer ships now; UI later. Records expose enough to drive any future UI surface (chat citations, retrieval-trace viewer, audit-log explorer).
- **Span-level attention attribution** — research territory, too expensive for production. The cascade verifier covers the production-grade middle ground.
- **Token-level streaming events** — too high-cardinality. Use spans + structured logs instead.
- **Full FActScore atomic decomposition on hot path** — offline only, optionally invoked from L03 eval harness.
- **Restate exactly-once for the audit write itself** — at-least-once + idempotency is sufficient and avoids latency on the durable workflow path.

## Stack additions

| Crate | Purpose | Crate name |
|---|---|---|
| `tokio` mpsc + `fjall` 3.x | Hot-path buffer + durable journal | (existing + `fjall`) |
| `tokio-postgres` COPY BINARY | Fast TimescaleDB writes | (existing) |
| `arrow-rs` + `parquet` + `object_store` | Parquet rolls to S3 | new |
| `tracing-opentelemetry` 0.32 + `opentelemetry-otlp` 0.31 | OTel GenAI emit | (existing — bumping versions if needed) |
| `openinference-semantic-conventions` | OpenInference attribute constants | new |
| `fastembed-rs` or `candle` + `ort` | Cascade NLI verifier (HHEM-2.1-open) | new |
| `tantivy` | BM25 for cascade Stage 1 | new |
| `blake3` | Audit hash primitive | new (compliance only) |
| `ct-merkle` | RFC 6962 Merkle log | new (compliance only) |
| `ed25519-dalek` | Merkle root signing | new (compliance only) |

TimescaleDB extension required on the existing Postgres 17 cluster.

## What this pack does NOT touch

- Memory subsystem (graph, vector, pii, ingest) is read-only context for this pack — emitters call into it but no schema changes.
- Session storage / pgaudit / S3 Object Lock infrastructure from M22 is reused by L04.
- moa-orchestrator and moa-orchestrator-local are touched in L01 only to wire the sink into `ToolContext` and emit at the right call sites.

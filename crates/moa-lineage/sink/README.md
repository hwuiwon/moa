# moa-lineage-sink

Durable lineage writer. Acceptance is a Postgres commit into
`analytics.lineage_journal`; a leased worker claims committed rows and lands
them transactionally in Postgres. The bounded hot-path mpsc channel is
best-effort ingress and a payload-free wake signal, never durability.
Also hosts the OTel span bridge formerly published as `moa-lineage-otel`.

## Structure

- `admin` — admin read helpers for hot lineage rows.
- `writer::acceptance` — the durable acceptance queue: commit, claim, lease,
  and dequeue.
- `clickhouse` — legacy ClickHouse read adapter still used by analytics/query
  consumers; the durable writer never writes through it.
- `error` — crate `Error`/`Result`.
- `mpsc_sink` — bounded hot-path mpsc `LineageSink` implementations
  (`MpscSink`, `NullSink`, `OtelSink`).
- `otel` — OpenTelemetry/OpenInference attribute emitters that annotate the
  current span in parallel with durable writes (absorbed from the former
  `moa-lineage-otel` crate).
- `store` — Postgres pool owned by the durable writer (`LineageStore`).
- `writer` — async lineage writer worker (`spawn_writer`, `WriterHandle`).

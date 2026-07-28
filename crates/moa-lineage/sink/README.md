# moa-lineage-sink

Durable lineage writer. Acceptance is a Postgres commit into
`analytics.lineage_journal`; a leased worker claims committed rows and lands
them in Postgres/TimescaleDB or ClickHouse. The bounded hot-path mpsc channel
is best-effort ingress and a payload-free wake signal, never durability. When
the ClickHouse backend is selected the high-volume `turn_lineage` stream moves
there while scores, dead letters, and compliance chain state stay in Postgres.
Also hosts the OTel span bridge formerly published as `moa-lineage-otel`.

## Structure

- `admin` — admin read helpers for hot lineage rows.
- `writer::acceptance` — the durable acceptance queue: commit, claim, lease,
  and dequeue.
- `clickhouse` — ClickHouse-backed store for high-volume `turn_lineage`
  rows.
- `error` — crate `Error`/`Result`.
- `mpsc_sink` — bounded hot-path mpsc `LineageSink` implementations
  (`MpscSink`, `NullSink`, `OtelSink`).
- `otel` — OpenTelemetry/OpenInference attribute emitters that annotate the
  current span in parallel with durable writes (absorbed from the former
  `moa-lineage-otel` crate).
- `schema` — schema installer for engineering-tier lineage.
- `store` — backend selection for durable lineage rows (`LineageStore`).
- `writer` — async lineage writer worker (`spawn_writer`, `WriterHandle`).

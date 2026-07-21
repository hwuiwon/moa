# moa-lineage-sink

Durable lineage writer: a bounded hot-path mpsc sink feeds a fjall journal,
and an async worker lands rows in Postgres/TimescaleDB or ClickHouse. The
backend switches on config presence — when `[clickhouse]` is configured, the
high-volume `turn_lineage` stream moves to ClickHouse while scores, dead
letters, and compliance chain state stay in Postgres. Also hosts the OTel
span bridge formerly published as `moa-lineage-otel`.

## Structure

- `admin` — admin read helpers for hot lineage rows.
- `clickhouse` — ClickHouse-backed store for high-volume `turn_lineage`
  rows.
- `error` — crate `Error`/`Result`.
- `fjall_journal` — durable fjall journal for pending lineage rows.
- `mpsc_sink` — bounded hot-path mpsc `LineageSink` implementations
  (`MpscSink`, `NullSink`, `OtelSink`).
- `otel` — OpenTelemetry/OpenInference attribute emitters that annotate the
  current span in parallel with durable writes (absorbed from the former
  `moa-lineage-otel` crate).
- `schema` — schema installer for engineering-tier lineage.
- `store` — backend selection for durable lineage rows (`LineageStore`).
- `writer` — async lineage writer worker (`spawn_writer`, `WriterHandle`).

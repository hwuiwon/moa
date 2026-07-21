# moa-memory-pii

PII classification and privacy enforcement for graph memory. Classifies text
into normalized `PiiCategory` values before durable memory writes, and owns
memory-side erasure and legal-hold fencing.

## Structure

- `openai_filter` — HTTP client for the out-of-process
  `openai/privacy-filter` inference service, with configurable thresholds.
- `mock` — mock PII classifier for deterministic ingestion tests.
- `erasure` — memory-owned privacy erasure helpers.
- `legal_hold` — linearizable legal-hold and destructive-operation fencing.

The crate root defines `PiiCategory` (person, email, phone, SSN, secret, ...)
with label parsing that normalizes common model output forms, plus the
mapping into `moa-core` sensitivity classes.

## Place In The Memory Family

Depends on `moa-memory-graph` (which in turn depends on
`moa-memory-vector`); `moa-memory-ingest` and `moa-memory-lifecycle` call
into this crate so nothing reaches durable memory unclassified.

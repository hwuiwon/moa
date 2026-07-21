# moa-memory-lifecycle

Maintenance passes for graph-memory lifecycle management: tenant
consolidation (decay, expiry, duplicate merging, contradiction sweeps),
outcome-weighted quality scoring, digest rendering, and skill lesson
curation. Shared by orchestrator workflows and eval runners.

## Structure

- `consolidate` — consolidation pass logic: confidence decay, idle-fact
  expiry, duplicate merging, contradiction sweeps, entity backfill, and
  per-tenant consolidation cursors.
- `quality` — outcome-weighted memory quality-score computation.
- `digest` — deterministic standing contact and tenant digest rendering and
  rebuilds.
- `curate` — ACE-style lesson curation for skill `Lesson` nodes.

## Place In The Memory Family

Consumes `moa-memory-graph`, `moa-memory-vector`, `moa-memory-pii`, and
`moa-memory-types`; it maintains stored memory rather than writing new facts
(that is `moa-memory-ingest`'s job).

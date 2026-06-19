# Findings: Lineage Linkage Hardening

## Audit Summary

- Production Restate context compilation passes `_moa.turn_seq` but not
  `_moa.turn_id`; graph memory retrieval lineage falls back to a fresh UUID.
- Production LLM completion persists `BrainResponse`, but does not emit
  `GenerationLineage` or `CitationLineage`.
- Harness lineage now emits context, generation, citation, and scores, but its
  context chunks use `session.id` as `source_uid`, so citations link to a
  synthetic chunk rather than the real memory node, tool event, or message row.
- `ContextMessage` does not carry provenance. `EventRecord` has `id` and
  `sequence_num`, but history conversion drops them before context lineage is
  built.
- Graph memory renders node UIDs into prompt text and records retrieval top-k
  UIDs, but there is no structured mapping from rendered memory chunks to the
  actual memory UIDs.
- `moa.retrieval_lineage` records `session_id`, `turn_seq`, `uid`, rank, and
  timestamp. It lacks `turn_id`, so it cannot directly join to
  `analytics.turn_lineage`.

## Architecture Constraints

- `docs/01-architecture-overview.md` defines `LineageHandle` as the
  transport-neutral capture seam and lists `analytics.turn_lineage` and
  `analytics.scores` as product-visible data.
- `docs/02-brain-orchestration.md` says `TurnExecution` owns the durable turn
  loop and is keyed by `turn_id`; this is the right production turn identity.
- `docs/04-memory-architecture.md` keeps retrieval sidecar writes
  fire-and-forget and flag-dark by default.
- `docs/05-session-event-log.md` makes session events append-only and gives
  event rows durable `id` and `sequence_num` fields.
- `docs/07-context-pipeline.md` requires memory to stay in the dynamic tail and
  preserve prompt-cache stability.

## Performance Notes

- The mpsc lineage sink is already request-path friendly: bounded `try_send`,
  background batching, and bulk writes.
- The expensive current pieces are pre-sink payload construction: cloning full
  context/source/answer text, serializing to JSON, then parsing JSON back into
  typed events for the Postgres sink.
- First fix should make linkage correct, then add metrics for citation
  verification duration, source count, answer bytes, and emitted payload bytes.

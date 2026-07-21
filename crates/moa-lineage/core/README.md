# moa-lineage-core

Core lineage data model and sink trait. This crate is the type-stable
foundation of the `moa-lineage` family: the other lineage subcrates depend on
it, and it depends only on `moa-core` for shared identity and scope types.

## Structure

- `chain` — shared canonical-payload hash-chain primitives for lineage
  writers and verifiers.
- `ids` — identifier newtypes for lineage records (`TurnId`).
- `records` — serializable lineage records emitted by retrieval, context,
  and generation (`LineageEvent`, `RetrievalLineage`, `ContextLineage`,
  `GenerationLineage`, decision records, scores).
- `sink` — the hot-path `LineageSink` trait implemented by
  `moa-lineage-sink`.

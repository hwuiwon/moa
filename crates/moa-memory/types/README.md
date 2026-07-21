# moa-memory-types

Memory-owned scope types and normalization contracts shared by retrieval,
ingestion, and storage crates. Single-module leaf crate with no storage or
provider dependencies.

## Contents

- `normalize_fact_component` — shared semantic contract for fact-content
  identity; retrieval selection, consolidation duplicate merging, and eval
  probes all compare facts through it.
- `normalize_entity_name` — deterministic blocking key for extracted entity
  mentions, shared by ingestion and lifecycle backfill.
- `FactCategory` — coarse semantic category decided once at extraction time
  (preference, identity, relationship, event, other).
- `FactEdgeLabel` — semantic graph edge label extraction assigns for a fact's
  object relationship.
- Scope types built on `moa-core` tenant/contact identifiers and
  `RlsContext`.

## Place In The Memory Family

The leaf of the family: depends only on `moa-core` and `serde`. Every other
`moa-memory-*` crate (and `moa-knowledge`) can depend on it without pulling
in storage or model providers.

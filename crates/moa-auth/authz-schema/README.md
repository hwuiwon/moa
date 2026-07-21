# moa-authz-schema

Typed OpenFGA authorization schema for MOA. This crate is the single source of
truth for object types, relations, and the deployed OpenFGA model; other crates
depend on it for compile-time checked tuple construction. It is a small leaf
crate with no async runtime or I/O.

## Structure

- `src/tuple.rs` — typed tuple building blocks: `ObjectType`, `Relation`,
  `UserType`, `TupleKey`/`TupleKeyWire`, and `TupleOp`.
- `src/schema_v1.json` — the reviewed OpenFGA JSON model, embedded as
  `SCHEMA_V1_JSON` and written to the server by `moa-fga-bootstrap`. Checked in
  because the schema is part of the security contract.
- `MODEL_VERSION` — logical schema version; outbox idempotency keys include it
  so tuples written under one version cannot be silently re-applied under the
  next.

## Rules

- Increment `MODEL_VERSION` on any change that adds, removes, or restructures
  relations.
- Relation changes hard-break stale relation names; old tuple semantics are not
  supported in parallel.

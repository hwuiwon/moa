# AGE Cypher Patterns

These rules apply to Cypher queries against Apache AGE. The most important rule is the first one.

## No String Formatting Into Cypher

- Never build a Cypher string by interpolating user-supplied values, node UIDs, label names, or property values.
- `format!("MATCH (n:{label}) ...", label = user_label)` is a Cypher injection bug, not a syntax convenience.
- Use parameterized Cypher. AGE supports `$param` parameter substitution; use the projection helper that the repo already provides rather than building strings by hand.
- For dynamic labels (rare), validate against an allowlist of known labels in Rust before composing the query, and never use the user-supplied string directly.

## Projection Helpers

The repo provides helpers that:

1. Project AGE rows into typed Rust structs.
2. Bind parameters to a `$1`-style placeholder list.
3. Coerce AGE's `agtype` representation to the typed shape the caller expects.

Find the current helper module before writing new Cypher; do not duplicate the projection logic. As of the audit the helpers live in `crates/moa-memory/graph/src/`, with names that include `cypher`, `project`, or `agtype`. Use the existing one.

## Workspace Scope in Cypher

- Workspace boundaries enforced by Postgres RLS do not apply to AGE graphs. Every Cypher query must include the workspace constraint explicitly:

  ```cypher
  MATCH (n {workspace_id: $workspace_id}) ...
  ```

- Forgetting the workspace clause in a Cypher query is a cross-tenant leak. There is no second line of defense at the AGE layer.
- For traversals across multiple hops, the constraint must apply to every intermediate node, not just the start node.

## Read vs Write Cypher

- Read queries should use `MATCH` and return projected rows.
- Write queries (`CREATE`, `MERGE`, `DELETE`) must run inside a transaction that also has the appropriate GUC scope set, because the changelog write that accompanies the graph mutation is RLS-scoped.

## When to Reach for Cypher vs SQL

- Use Cypher when the query is intrinsically graph-shaped: traversals, neighborhoods, paths.
- Use SQL when the query is row-shaped: filtering by `valid_from`/`valid_to`, joining changelog records, paginating by `created_at`.
- Many memory operations are hybrid (graph + SQL). The repo's pattern is one transaction with both AGE and SQL operations; do not split into two transactions and trust them to converge.

## Where to Look

- `crates/moa-memory/graph/src/` for the current Cypher helpers
- `crates/moa-memory/graph/tests/` for examples of correctly-scoped queries
- `docs/04-memory-architecture.md` for the conceptual model

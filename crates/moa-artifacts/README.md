# moa-artifacts

Canonical artifact definitions for MOA agents, skills, connectors, actions,
and experiment plans. The crate owns the code-addressable document model used
by API imports, Postgres storage, and future visual builders; runtime crates
depend on these types instead of duplicating ad hoc JSON shapes.

## Structure

- `action.rs` — standalone action artifact definitions.
- `agent.rs` — tenant-configurable agent artifact definitions.
- `canonical.rs` — canonical JSON serialization and hashing helpers.
- `connector.rs` — connector artifact definitions.
- `document.rs` — artifact document wrappers and metadata.
- `execution_plan.rs` — canonical execution-plan, goal, outcome, and amendment
  definitions.
- `reference.rs` — stable artifact reference parsing and formatting.
- `registry.rs` — Postgres-backed artifact registry.
- `resolver.rs` — reference resolution against published artifact revisions.
- `simulation.rs` — behavior-lab experiment plan and embedded simulation
  definitions.
- `skill.rs` — skill artifact definitions.
- `validation.rs` — semantic validation for artifact documents.

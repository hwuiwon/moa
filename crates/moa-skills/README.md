# moa-skills

Agent Skill package parsing, registry, rendering, and skill learning
(distillation, improvement, and the reinforcement loop) for MOA. See
`docs/09-skills-and-learning.md` for the architecture.

## Structure

- `format.rs` — Agent Skill markdown parsing and rendering utilities.
- `package.rs` — skill package validation and deterministic package metadata.
- `registry.rs` — artifact-backed skill package registry with three-tier
  scoping.
- `artifact.rs` — conversion between skill packages and canonical skill
  artifacts.
- `render.rs` — skill rendering with linked graph lessons.
- `lessons.rs` — skill lesson graph helpers.
- `distiller.rs` — skill distillation from successful agent runs.
- `improver.rs` — existing-skill self-improvement draft generation.
- `candidates.rs` — learning-candidate helpers for creation and improvement
  proposals.
- `proposals.rs` — draft artifact proposal storage for self-generated skill
  packages.
- `review.rs` — application helpers for reviewing generated learning
  candidates.
- `mining.rs` — deterministic weakness mining from eval and session failure
  signals.
- `recurrence.rs` — exact-fingerprint recurrence qualification for skill
  learning.
- `embeddings.rs` — background backfill of semantic embeddings for the
  skill-reinforcement loop.
- `semantic.rs` — pure decision logic for the semantic layer of the
  skill-learning loop.
- `regression.rs` — skill regression suite source generation and comparison.
- `rollback.rs` — post-promotion skill-regression detection and
  rollback-proposal filing.

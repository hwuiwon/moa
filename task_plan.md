# Lineage Linkage Hardening

## Goal

Make production lineage link answers to the exact turn, compiled context chunks,
and source objects/messages that influenced them, while keeping request-path
latency bounded.

## Current Status

- Phase 1: Audit and scope current lineage gaps - complete
- Phase 2: Create durable plan artifacts - complete
- Phase 3: Thread production turn identifiers into context/retrieval lineage - complete
- Phase 4: Add structured context source references - complete
- Phase 5: Emit production generation and citation lineage - complete
- Phase 6: Add focused tests and migration/docs updates - complete
- Phase 7: Run verification and record performance risks - complete

## Constraints

- Preserve the existing modular monolith; do not add a service boundary.
- Keep lineage capture best-effort on the hot path.
- Do not revert existing eval/harness lineage changes.
- Prefer typed source references over parsing rendered prompt text.
- Keep changes focused on the lineage contract rather than rewriting memory
  retrieval quality scoring.

## Decisions

- Use the Restate workflow key as the production `turn_id`.
- Make context chunks carry source references for session events, graph memory,
  tool calls/results, and synthetic prompt sections where known.
- Reuse the existing citation verifier for the first production pass.
- Add timing/size metrics before optimizing the verifier or serialization path.
- Add `turn_id` to the retrieval sidecar with a nullable migration so older rows
  remain readable.

## Plan Artifact

- `docs/engineering-discipline/plans/2026-06-19-lineage-linkage.md`

## Errors Encountered

| Error | Attempt | Resolution |
|---|---|---|
| Existing root planning files described a completed action-policy task | Initial planning setup | Replace active root files for the new lineage work and keep a dated plan artifact |

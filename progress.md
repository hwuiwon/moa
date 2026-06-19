# Progress: Lineage Linkage Hardening

## 2026-06-19

- Read planning-with-files, Rust, and test-authoring skill instructions.
- Ran planning session catchup; no unsynced recovery output was reported.
- Read `docs/01-architecture-overview.md`, `docs/02-brain-orchestration.md`,
  `docs/04-memory-architecture.md`, `docs/05-session-event-log.md`, and
  `docs/07-context-pipeline.md`.
- Confirmed existing uncommitted eval/harness lineage changes are present and
  should be preserved.
- Replaced completed action-policy root planning files with active lineage
  planning files.
- Created executable plan at
  `docs/engineering-discipline/plans/2026-06-19-lineage-linkage.md`.
- Started Task 1: durable turn-id propagation in the production Restate path.
- Patched `prepare_turn_request` to accept a typed `TurnId`, insert
  `_moa.turn_id` before pipeline execution, and pass the workflow key from
  `TurnExecution`.
- Ran `cargo check -p moa-orchestrator --tests --locked`; it passed.
- Added nullable `turn_id` to retrieval sidecar lineage context and writer SQL.
- Added migration
  `crates/moa-migrations/migrations/postgres/V000305__retrieval_lineage_turn_id.sql`
  and registered it in `moa-migrations`.
- Added citation/generation answer-event id and sequence fields; production
  `TurnExecution` now does a bounded recent `BrainResponse` lookup and attaches
  the matching event record when found.
- Added citation verifier overhead metrics for source count, answer bytes, and
  verifier duration.
- Updated architecture docs for production lineage linkage, retrieval sidecar
  `turn_id`, and context source refs.
- Ran focused unit tests:
  - `cargo test -p moa-core --locked context_message_source_refs_are_preserved -- --nocapture`
  - `cargo test -p moa-brain --features eval-harness --locked context_chunk_preserves_structured_source_refs -- --nocapture`
  - `cargo test -p moa-brain --locked lineage_context_uses_compiled_turn_id_metadata -- --nocapture`
  - `cargo test -p moa-brain --features eval-harness --locked citation_lineage_cites_context_source_for_answer -- --nocapture`
- Mutation-verified `context_chunk_preserves_structured_source_refs` by
  temporarily forcing chunk `source_uid` to the session id; the test failed on
  the source UID assertion, then passed after revert.
- Ran verification:
  - `cargo fmt --all`
  - `cargo check -p moa-brain --features eval-harness --tests --locked`
  - `cargo check -p moa-orchestrator --tests --locked`
  - `cargo check -p moa-migrations --tests --locked`
  - `cargo check -p moa-eval --tests --locked`
  - `cargo run -p xtask -- check-migrations`
  - `cargo clippy -p moa-brain --features eval-harness --tests --locked -- -D warnings`
  - `cargo clippy -p moa-orchestrator --tests --locked -- -D warnings`
  - `cargo clippy -p moa-migrations --tests --locked -- -D warnings`
  - `git diff --check`
- Started Task 2: structured source refs for context lineage.
- Added `ContextSourceKind` and `ContextSourceRef` to `moa-core`, plus
  `ContextMessage.source_refs` and builder helpers.
- Added `ContextChunk.source_refs` in `moa-lineage-core`.
- Preserved event/tool source refs during history conversion and compacted error
  replay.
- Added graph-memory source refs when the memory retriever inserts the rendered
  memory reminder.
- Updated context lineage chunk construction to copy refs and use the first real
  source UID as the legacy `source_uid`.
- Ran `cargo check -p moa-brain --features eval-harness --tests --locked`; it
  passed.
- Ran `cargo check -p moa-orchestrator --tests --locked`; it passed.
- Started Task 3: production generation/citation lineage emission.
- Promoted the harness lineage helpers to shared `moa_brain::lineage`.
- Added `ChunkRef` serde derives and direct manifest dependencies where needed.
- Production request preparation now emits `ContextLineage` and carries citable
  chunks through `PreparedTurnRequestOutput`.
- `TurnExecution` now emits `GenerationLineage`, `CitationLineage`, and related
  scores after the LLM gateway returns.
- Fixed missing `serde` and `moa-lineage-citation` manifest dependencies found
  by focused checks.
- Ran `cargo check -p moa-brain --features eval-harness --tests --locked`; it
  passed.
- Ran `cargo check -p moa-orchestrator --tests --locked`; it passed.

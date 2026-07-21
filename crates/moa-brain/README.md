# moa-brain

Context compilation, execution planning, and turn helpers for MOA. This crate
turns session state and graph memory into model-ready context and drives a
single agent turn end to end. Retrieval and query planning live in
`moa-retrieval`; this crate consumes them through the context pipeline.

## Modules

- `pipeline` — context pipeline runner, processors, and stage assembly.
- `execution_planning` — bounded model-assisted routing plus strict
  execution-plan generation.
- `harness` (feature `eval-harness`) — single-turn brain harness execution and
  the shared streamed turn engine.
- `turn` — shared streamed-turn helpers used by the buffered harness and the
  Restate orchestrator.
- `learning` — segment-derived learning artifacts for MOA's self-improvement
  loop.
- `turn_learning` — construction helpers for segment-derived learning
  artifacts.
- `segment_assessment` / `turn_segments` — automated task-segment assessment
  and its pure per-turn helpers.
- `compaction` — reversible session-history compaction helpers.
- `query_rewrite` — query-rewriting metadata shared across context pipeline
  stages.
- `lineage` — lineage emission helpers for streamed turns and production turn
  workflows.
- `runtime_events` — live runtime event types used by local UI surfaces.

## Features

- `eval-harness` — enables the `harness` module, the `chat_harness` /
  `replay_corpus` examples, and the harness-driven test binaries; pulls in
  `moa-hands` and `moa-memory-ingest`.

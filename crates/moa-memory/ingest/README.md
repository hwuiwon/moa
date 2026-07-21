# moa-memory-ingest

Graph-memory ingestion pipelines. Owns the Restate `IngestionVO` slow path
(LLM-backed fact extraction, entity resolution, contradiction detection) and
the inline fast path for explicit remember/forget/supersede commands.

## Structure

- `slow_path` — Restate virtual object for slow-path graph-memory ingestion.
- `fast_path` — low-latency ingestion for explicit remember, forget, and
  supersede commands.
- `chunking` — sentence-aware transcript chunking for slow-path ingestion.
- `extract` — deterministic DTOs and extract/chunk helpers.
- `extractor` — fact extractor seam (`FactExtractor`, heuristic fallback).
- `model_fact_extractor` — model-backed fact extraction through the seam.
- `model_entity_merge` — model-backed entity merge verification with recorded
  replay support.
- `entity_resolution` — entity resolution helpers for slow-path ingestion.
- `contradiction` — hybrid contradiction detection for incoming facts.
- `recorded` — recorded fact extraction replay for hermetic memory-eval lanes.
- `ctx` — runtime context installed by hosts that execute ingestion.
- `error` — error types shared by ingestion helpers.

## Features

- `test-util` — exposes scripted/recorded extractor test doubles; required by
  the `extract` and `slow_path_orchestration_db_memory` test binaries.

## Place In The Memory Family

Sits at the top of the memory chain: depends on `moa-memory-graph`,
`moa-memory-pii`, `moa-memory-vector`, and `moa-memory-types`, plus
`moa-providers` for model calls — this is the LLM-heavy slow path. It is an
optional dependency of `moa-brain` behind the `eval-harness` feature.

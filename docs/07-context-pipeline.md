# 07 — Context Compilation Pipeline

_Ordered processors, query rewriting, skill injection, memory retrieval, compaction, and cache stability._

## Purpose

The context pipeline turns durable session state into one provider request. It must balance four goals:

- preserve prompt-cache stability
- include task-relevant memory and skills
- keep history within budget
- produce metadata for task segmentation and learning

The implementation lives in `crates/moa-brain/src/pipeline/`.

## Current Stage Order

The code reports fixed stage numbers through each `ContextProcessor`. With query rewriting enabled, the default pipeline contains ten processors:

| Stage | Processor | Cache role | Purpose |
|---|---|---|---|
| 1 | `IdentityProcessor` | Stable prefix | MOA identity and high-level behavior |
| 2 | `InstructionProcessor` | Stable prefix | user/workspace instructions |
| 3 | `ToolDefinitionProcessor` | Stable prefix | deterministic tool schema list, capped at 30 |
| 4 | `SkillInjector` | Stable prefix breakpoint | budgeted skill manifest ranked for the task |
| 5 | `QueryRewriter` | Dynamic metadata | rewritten query, high-level intent, clarification flag, task transition flag |
| 6 | `MemoryRetriever` | Dynamic tail | user/workspace indexes and relevant memory pages |
| 7 | `HistoryCompiler` | Dynamic/history prefix | replayed events, checkpoints, recent turns, errors |
| 8 | `RuntimeContextProcessor` | Dynamic tail | current date, workspace, working directory, branch, user |
| 9 | `Compactor` | Dynamic maintenance | checkpoint/compaction when thresholds are exceeded |
| 10 | `CacheOptimizer` | Final pass | cache breakpoints and provider-specific cache metadata |

If query rewriting is disabled, the stage-5 processor is omitted and the pipeline has nine processors; later processors keep their configured stage numbers.

## Stable Prefix

The stable prefix is produced by stages 1-4. These stages avoid per-turn values such as timestamps, working directory, branch, counters, or usage stats that would break byte-stable prompt caching.

`SkillInjector` marks a one-hour cache breakpoint after the skill manifest. It sorts and budgets the manifest so the stable prefix remains deterministic for the same inputs.

## Query Rewriting

`QueryRewriter` is fail-open. On timeout, parsing error, circuit-breaker open, or skipped input, it stores a passthrough `QueryRewriteResult` and lets the turn continue.

The rewriter produces:

- `rewritten_query`
- high-level `intent`
- `sub_queries`
- `suggested_tools`
- `needs_clarification`
- `clarification_question`
- `is_new_task`
- `task_summary`
- `source`

`is_new_task` and `task_summary` feed the segment tracker. The rewritten query feeds memory retrieval.

## Skill Injection

`SkillInjector` loads workspace skill metadata from memory and ranks skills with:

- keyword overlap against the current query
- tenant-level skill resolution rates from `skill_resolution_rates`
- normalized use count
- recency

It emits only a compact manifest. Full skill bodies are loaded later through memory tools when activated. The manifest is budget-aware through `SkillBudgetConfig`.

## Memory Retrieval

`MemoryRetriever` loads ranked graph hits through the graph, sidecar, and vector
memory crates. See
`docs/architecture/decisions/0001-envelope-encryption-deferred.md` for the
current privacy boundary and `crates/moa-memory/README.md` for crate-level
details.

Search uses the rewritten query when available, otherwise extracted keywords from the latest user message. Retrieval can be keyword, semantic, or hybrid depending on the memory store configuration.

Memory is inserted as a reminder near the active turn so runtime facts and retrieved context do not disturb the stable prefix.

## History Compilation

`HistoryCompiler` reads durable events from `SessionStore`, applies checkpoints and context snapshots when available, preserves recent turns, and keeps errors visible. It is segment-aware because `SegmentStarted` and `SegmentCompleted` events remain in the replay stream.

## Runtime Context

`RuntimeContextProcessor` inserts volatile facts at the end of the prompt:

- current date
- workspace
- current working directory
- git branch
- user

These values are intentionally outside the stable prefix.

## Compaction

`Compactor` watches event and token thresholds. When compaction is needed, it can ask an LLM for a checkpoint summary, persist a `Checkpoint` event, and let future history compilation start from a compact representation while preserving durable history.

## Cache Optimizer

`CacheOptimizer` finalizes provider cache hints and records cache metrics. Prompt-cache rules are documented in `prompt-caching-architecture.md`.

## Observability

Each processor returns `ProcessorOutput` with:

- tokens added and removed
- included and excluded items
- excluded item details
- duration
- metadata

The pipeline records structured tracing spans with session, user, workspace, model, stage number, stage name, token counts, and cache metrics.

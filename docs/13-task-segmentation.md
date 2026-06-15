# 13 — Task Segmentation

_Segment lifecycle, outcome assessment, and segment analytics._

## Purpose

A session can contain multiple tasks. MOA tracks each task as a segment so learning is based on discrete outcomes instead of whole-session guesses.

Segments answer:

- What task was being attempted?
- Which tools and skills were used?
- How many turns and tokens did it cost?
- What outcome was assessed?
- What learning should be recorded from the outcome?

## Data Model

`TaskSegment` lives in `crates/moa-core/src/types/segments.rs`; persistent rows live in Postgres `task_segments`.

Important fields:

- `id`
- `session_id`
- `tenant_id`
- `segment_index`
- `task_summary`
- `started_at`
- `ended_at`
- `outcome`
- `assessment`
- `outcome_confidence`
- `tools_used`
- `skills_activated`
- `turn_count`
- `token_cost`
- `previous_segment_id`

`ActiveSegment` is the lighter projection stored in session VO state.

## Segment Detection

Query rewriting produces `QueryRewriteResult`:

- `retrieval_query`
- `source`
- `reason`
- `is_new_task`
- `task_summary`

When a turn is prepared, `SegmentTracker` uses the query rewrite metadata and session events to decide whether to:

- keep the current active segment
- create the first segment
- close the previous segment and start a new one

Segment tracking reads only `is_new_task` and `task_summary`. Query rewrite metadata does not define a durable session intent taxonomy and does not choose tools for the agent.

The event log records `SegmentStarted` and `SegmentCompleted` events.

## Segment Counters

During a turn, the orchestrator records:

- tool names used
- skill names activated
- completed turn count
- token cost

The active VO state and `task_segments` row stay in sync through session store calls.

## Segment Assessment

Segment assessment runs after boundaries such as segment completion, idle
turns, cancellation, timeout, or deferred continuation evidence. It does not
decide whether the live agent loop continues; deterministic turn state,
approvals, cancellation, queued messages, and tool events own that control
path.

Assessment combines five signal classes:

| Signal | Meaning |
|---|---|
| Tool outcome | Whether tools completed, failed, or produced useful output |
| Verification | Whether tests/checks/verification commands succeeded |
| Continuation | Whether the next user message indicates success, rework, abandonment, or a new task |
| Self-assessment | Whether the agent response claims completion or uncertainty |
| Structural | Whether turns, cost, and duration are anomalous for the tenant baseline |

The assessor outputs:

- `resolved`
- `partial`
- `unknown`
- `failed`
- `abandoned`

Assessment phases:

- `immediate`: when a segment appears idle or completed
- `deferred`: after a later user message gives continuation evidence
- `final`: when cancellation or timeout closes the segment

Each assessment updates the segment row and appends `segment_assessed` to `learning_log`.

## Materialized Views

Segment rows drive learning views:

| View | Use |
|---|---|
| `skill_resolution_rates` | Ranks skills by tenant-level resolution outcomes |
| `segment_baselines` | Provides structural baselines for segment assessment |

Refresh is handled through the session store's materialized-view refresh path.

## Compaction Interaction

Segment events are durable boundaries. History compaction can summarize older events, but segment start/completion records remain part of replay and analytics.

## Learning Flow

```text
User messages
  -> query rewrite
  -> segment start/continue/complete
  -> tool and skill counters
  -> segment assessment
  -> learning_log
  -> skill ranking and memory learning
```

Task segmentation is the measurement layer that makes the rest of MOA's learning pipeline reliable.

# 13 — Task Segmentation

_Segment lifecycle, resolution scoring, and segment analytics._

## Purpose

A session can contain multiple tasks. MOA tracks each task as a segment so learning is based on discrete outcomes instead of whole-session guesses.

Segments answer:

- What task was being attempted?
- Which intent was assigned, if any?
- Which tools and skills were used?
- How many turns and tokens did it cost?
- Did it resolve?
- What learning should be recorded from the outcome?

## Data Model

`TaskSegment` lives in `crates/moa-core/src/types/segments.rs`; persistent rows live in Postgres `task_segments`.

Important fields:

- `id`
- `session_id`
- `tenant_id`
- `segment_index`
- `intent_label`
- `intent_confidence`
- `task_summary`
- `started_at`
- `ended_at`
- `resolution`
- `resolution_signal`
- `resolution_confidence`
- `tools_used`
- `skills_activated`
- `turn_count`
- `token_cost`
- `previous_segment_id`

`ActiveSegment` is the lighter projection stored in session VO state.

## Segment Detection

Query rewriting produces `QueryRewriteResult`:

- `rewritten_query`
- high-level `intent`
- `is_new_task`
- `task_summary`
- suggested tools and clarification metadata

When a turn is prepared, `SegmentTracker` uses the query rewrite metadata and session events to decide whether to:

- keep the current active segment
- create the first segment
- close the previous segment and start a new one

The event log records `SegmentStarted` and `SegmentCompleted` events.

## Intent Classification

New segments can be classified against active tenant intents:

1. Build text from task summary and first user message.
2. Embed the text with the configured embedding provider.
3. Query active tenant intent centroids in Postgres.
4. Accept the nearest match when cosine distance is below the configured threshold.
5. Store the label and confidence on the segment.
6. Append `intent_classified` to `learning_log`.

If a tenant has no active intents, classification returns no match. New tenants therefore start blank.

## Segment Counters

During a turn, the orchestrator records:

- tool names used
- skill names activated
- completed turn count
- token cost

The active VO state and `task_segments` row stay in sync through session store calls.

## Resolution Scoring

Resolution scoring combines five signal classes:

| Signal | Meaning |
|---|---|
| Tool outcome | Whether tools completed, failed, or produced useful output |
| Verification | Whether tests/checks/verification commands succeeded |
| Continuation | Whether the next user message indicates success, rework, abandonment, or a new task |
| Self-assessment | Whether the agent response claims completion or uncertainty |
| Structural | Whether turns, cost, and duration are anomalous for the tenant/intent baseline |

The scorer outputs:

- `resolved`
- `partial`
- `unknown`
- `failed`
- `abandoned`

Scoring phases:

- `immediate`: when a segment appears idle or completed
- `deferred`: after a later user message gives continuation evidence
- `final`: when cancellation or timeout closes the segment

Each score updates the segment row and appends `resolution_scored` to `learning_log`.

## Materialized Views

Segment rows drive learning views:

| View | Use |
|---|---|
| `skill_resolution_rates` | Ranks skills by tenant/intent resolution outcomes |
| `intent_transitions` | Tracks common task-to-task flows |
| `segment_baselines` | Provides structural baselines for resolution scoring |

Refresh is handled through the session store's materialized-view refresh path.

## Compaction Interaction

Segment events are durable boundaries. History compaction can summarize older events, but segment start/completion records remain part of replay and analytics.

## Learning Flow

```text
User messages
  -> query rewrite
  -> segment start/continue/complete
  -> tool and skill counters
  -> resolution score
  -> learning_log
  -> skill ranking and intent learning
```

Task segmentation is the measurement layer that makes the rest of MOA's learning pipeline reliable.

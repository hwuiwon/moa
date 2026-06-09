# 17 - Observability

_Turn latency spans, broadcast lag, metrics, and fast interpretation._

## Turn Latency

Each `session_turn` trace emits four named child spans:

```text
session_turn
├── pipeline_compile
├── llm_call
├── tool_dispatch
└── event_persist
```

| Span | Covers |
|---|---|
| `pipeline_compile` | Full context pipeline build. Processor spans such as `history_compiler` remain nested under it. |
| `llm_call` | Provider request and streamed response lifetime, including TTFT. |
| `tool_dispatch` | Tool-call coordination for the turn. Individual tools appear as spans such as `tool:file_read`. |
| `event_persist` | Turn commit overhead: event writes, status updates, and post-turn store updates. |

The `session_turn` root span records:

- `moa.turn.pipeline_compile_ms`
- `moa.turn.llm_call_ms`
- `moa.turn.tool_dispatch_ms`
- `moa.turn.event_persist_ms`
- `moa.turn.llm_ttft_ms`

The `llm_call` span records model, usage, cache token counts, first-token time,
and stream duration through `gen_ai.*` and `moa.llm.*` attributes.

Healthy trace shape:

```text
session_turn
├── pipeline_compile
│   ├── identity_processor
│   ├── instruction_processor
│   ├── tool_definition_processor
│   ├── skill_injector
│   ├── memory_retriever
│   ├── history_compiler
│   └── cache_optimizer
├── llm_call
├── tool_dispatch
└── event_persist
```

Fast interpretation:

- `llm_call` dominates: model/provider latency is the primary lever.
- `pipeline_compile` grows over a session: inspect replay and compiled context
  size.
- `tool_dispatch` dominates: inspect shell commands, file scans, or repeated
  tool loops.
- `event_persist` is high: inspect session-store writes and post-turn
  maintenance.

## Broadcast Lag

MOA uses Tokio broadcast channels for live session updates:

- `event_tx` for persisted session-event previews;
- `runtime_tx` for live runtime updates used by gateway/API observers.

When a subscriber falls behind, Tokio returns `RecvError::Lagged(n)`. MOA does
not treat that as fatal for best-effort live previews.

Signals to watch:

- warn logs containing `broadcast subscriber fell behind, dropped events`;
- `moa_broadcast_lag_events_dropped_total`;
- `moa_broadcast_lag_events_dropped_by_channel_total`.

Important labels:

- `channel=event`
- `channel=runtime`
- `session_id=<uuid>` on the high-cardinality counter

Runtime behavior:

| Policy | Behavior | Use |
|---|---|---|
| `SkipWithGap` | Emit a gap marker and refresh from durable session log | Gateway/API observers |
| `BackfillFromStore` | Reload from `SessionStore::get_events` after last sequence | Complete ordered consumers |
| `Abort` | Stop the consumer | Automated observers that are cheaper to restart |

Interpretation:

- high `event` lag means the event-preview subscriber is slow or the buffer is
  undersized;
- high `runtime` lag means a live UI or relay subscriber is not draining fast
  enough;
- zero counters under normal load means there is no reason to increase channel
  sizes.

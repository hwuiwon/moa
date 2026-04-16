<!--
Turn latency decomposition for session traces.
-->

# Turn Latency Decomposition

Each `session_turn` trace now emits four named child spans so turn wall clock can
be decomposed without reconstructing it from low-level events:

```text
session_turn
├── pipeline_compile
├── llm_call
├── tool_dispatch
└── event_persist
```

## What each span covers

- `pipeline_compile`
  - The full context pipeline build for the turn.
  - Existing processor spans such as `history_compiler` remain nested under this
    span.
- `llm_call`
  - The provider request plus the full streamed response lifetime.
  - Includes TTFT via `gen_ai.response.first_token_at_ms`.
- `tool_dispatch`
  - All tool-call coordination for the turn.
  - Individual tool spans are exported as `tool:<name>`, for example
    `tool:file_read` or `tool:str_replace`.
- `event_persist`
  - Turn commit overhead: event writes, status updates, and other post-turn store
    updates.

## Span attributes

The `session_turn` root span records these aggregate fields:

- `moa.turn.pipeline_compile_ms`
- `moa.turn.llm_call_ms`
- `moa.turn.tool_dispatch_ms`
- `moa.turn.event_persist_ms`
- `moa.turn.llm_ttft_ms`

The `llm_call` span also records:

- `gen_ai.request.model`
- `gen_ai.usage.input_tokens`
- `gen_ai.usage.output_tokens`
- `gen_ai.usage.cache_read_tokens`
- `gen_ai.usage.cache_write_tokens`
- `gen_ai.response.first_token_at_ms`
- `moa.llm.stream_duration_ms`

## Expected trace shape

In Jaeger or Tempo, a healthy turn should look approximately like:

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
│   └── anthropic_messages_create
├── tool_dispatch
│   ├── tool:file_read
│   ├── tool:grep
│   └── tool:str_replace
└── event_persist
```

## Fast interpretation

- If `llm_call` dominates, model latency is the primary lever.
- If `pipeline_compile` grows turn over turn, inspect event replay and compiled
  context size.
- If `tool_dispatch` dominates, look for expensive shell commands or repeated
  file scans.
- If `event_persist` is unexpectedly high, inspect session store writes and
  post-turn maintenance work.

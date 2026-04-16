# Step 81 — Turn Latency Decomposition Spans

_Split the existing `session_turn` span into named sub-spans so we can measure what fraction of a turn's wall clock is LLM call vs context compile vs tool exec vs event persist. Answers: "where is the time actually going?"_

---

## 1. What this step is about

Steps 39–41 added OTel spans on LLM provider calls, individual tool calls, and individual pipeline stages. We have spans at the leaves. We do not have clean aggregated sub-span timers on the turn.

The research predicts the breakdown to be roughly: 70–85% LLM, 5–15% tool exec, 2–5% context compile, 1–3% event log. If MOA deviates significantly, we need to see it. For example: if context compile is 20% because of step 80's O(N²) replay, that's a clear signal to prioritize step 86 (snapshot). If it's 3%, we can skip straight to caching work.

This is a visibility step, not a fix step.

---

## 2. Files to read

- `moa-orchestrator/src/local.rs:run_session_task` — the outer turn loop. Phases are already logically separated but not timed as span children.
- `moa-brain/src/harness.rs:run_streamed_turn_with_signals_stepwise` — where the LLM call and tool dispatch happen.
- `moa-brain/src/pipeline/*.rs` — already has per-stage spans from step 40.
- `moa-providers/src/*.rs` — already has provider-level spans from step 39.

---

## 3. Goal

Every turn produces an OTel trace shaped like:

```
session_turn [root]
├── pipeline_compile              (stage 6 of this is the biggest child, already instrumented)
│   ├── identity_processor
│   ├── instruction_processor
│   ├── tool_definition_processor
│   ├── skill_injector
│   ├── memory_retriever
│   ├── history_compiler
│   └── cache_optimizer
├── llm_call                      (wraps LLMProvider::complete + streaming)
│   └── anthropic_messages_create (existing provider span)
├── tool_dispatch                 (NEW — covers all tool calls in this turn)
│   ├── tool:file_read
│   ├── tool:str_replace
│   └── tool:bash
└── event_persist                 (NEW — covers emit_event + post-turn DB writes)
```

Each sub-span's duration is visible in Jaeger/Tempo. We can sum durations per category per session and compute percentages.

---

## 4. Rules

- **No new libraries.** `tracing` spans are already everywhere. This is just reshaping existing instrumentation.
- **Span boundaries must cover the full wall-clock of that phase.** If `llm_call` starts after the stream starts producing tokens, we miss TTFT. Start the span before `LLMProvider::complete` is called.
- **Tool dispatch is a single span with multiple children.** If a turn makes 3 tool calls, `tool_dispatch` is one span whose duration covers all 3 calls and the coordination overhead between them.
- **Event persist covers both the per-event `emit_event` calls and the post-turn `refresh_workspace_tool_stats` / `update_status` writes.** These are the "cost-to-commit-the-turn" overhead.
- **Don't invent sub-spans that aren't meaningful.** `signal_check` ticks are noise at <100µs each. Leave them untraced.
- **Preserve parent-child relationships.** The existing provider and pipeline spans must become children of the new sub-spans, not siblings. Use `#[instrument]` or explicit `.in_scope` / `.instrument` so context propagates.

---

## 5. Tasks

### 5a. Introduce four named sub-spans in `run_session_task`

In the current loop body:

```rust
let pipeline_compile_span = tracing::info_span!("pipeline_compile");
let compiled = pipeline.compile(...).instrument(pipeline_compile_span.clone()).await?;

let llm_call_span = tracing::info_span!("llm_call",
    otel.kind = "client",
    gen_ai.operation.name = "chat",
    gen_ai.request.model = %llm_provider.model_id(),
);
let response_stream = llm_provider.complete(request)
    .instrument(llm_call_span.clone())
    .await?;

let tool_dispatch_span = tracing::info_span!("tool_dispatch");
// Tool dispatch already happens per-call inside run_streamed_turn_with_signals_stepwise.
// The simplest wiring is to pass the span into that function and ensure each tool call
// is instrumented as a child of it.

let event_persist_span = tracing::info_span!("event_persist");
// Wrap the post-turn persistence block (update_status, refresh_workspace_tool_stats,
// maybe_distill_skill, etc.) in .instrument(event_persist_span.clone())
```

All four are children of the existing `session_turn` root span.

### 5b. Make LLM span cover TTFT

For a streaming provider, TTFT is the time between "request sent" and "first content delta arrives." Add a span event:

```rust
let span = tracing::Span::current();
span.record("gen_ai.response.first_token_at_ms", ttft_ms);
```

Emit this the moment the first content block is observed on the stream. Then we can compute TTFT from span data without a separate span.

### 5c. Tool dispatch sub-span wiring

`run_streamed_turn_with_signals_stepwise` currently dispatches tool calls inline. To get them all under one parent `tool_dispatch` span, we need that function to know the parent span. Two options:

- **Option A (preferred):** Accept a `&tracing::Span` parameter, and inside the function do `let _enter = tool_dispatch_span.enter();` around the tool dispatch block.
- **Option B:** Use `tokio::task_local!` to stash the parent span; tool dispatch looks it up. Less plumbing but harder to reason about.

Pick A. It's an extra parameter on one function.

### 5d. Named tool spans

Existing tool call spans from step 40 may be named generically (e.g., `tool_call`). Rename to include the tool name: `tool:file_read`, `tool:str_replace`, etc. Jaeger's UI groups by span name, so this change makes it trivial to filter "show me all file_read calls across the trace."

### 5e. Span attributes that matter for aggregation

On each sub-span, record attributes that let us slice the data:

```
pipeline_compile:
  moa.pipeline.stages = 7
  moa.pipeline.total_tokens = <ctx.token_count>

llm_call:
  gen_ai.request.model
  gen_ai.usage.input_tokens / output_tokens / cache_read_tokens (step 79)
  gen_ai.response.first_token_at_ms
  moa.llm.stream_duration_ms

tool_dispatch:
  moa.tool.count                 (number of tool calls in this turn)
  moa.tool.parallel_count        (if any provider-side parallel tool calls; 0 for now)

event_persist:
  moa.persist.events_written     (count of emit_event during this turn)
```

### 5f. Per-turn summary metric

After the four sub-spans close, emit one summary line:

```rust
tracing::info!(
    turn_number,
    pipeline_compile_ms,
    llm_call_ms,
    tool_dispatch_ms,
    event_persist_ms,
    llm_ttft_ms,
    "turn latency breakdown"
);
```

This is the flat log format. Aggregation can be done via log queries or via OTel metrics (prefer the span duration histogram route).

### 5g. Tests

- Extend the step 78 integration test. After the multi-turn session, assert that trace export captured at least one `pipeline_compile`, one `llm_call`, one `tool_dispatch`, and one `event_persist` span per turn.
- Use a `tracing-subscriber::fmt` test collector or `tracing_test::traced_test` to capture spans.

---

## 6. Deliverables

- [ ] Four new named sub-spans on every turn: `pipeline_compile`, `llm_call`, `tool_dispatch`, `event_persist`.
- [ ] LLM call span records TTFT as an attribute.
- [ ] Tool call spans renamed to include tool name.
- [ ] Per-turn summary log line with four duration fields.
- [ ] Integration test verifies span presence.
- [ ] Short doc at `moa/docs/observability/turn-latency.md` with screenshots (or ASCII examples) of expected Jaeger/Tempo view.

---

## 7. Acceptance criteria

1. Running `moa exec` with OTel collector attached shows, in Jaeger, a turn trace with exactly the span tree described in section 3.
2. LLM span duration is >= pipeline compile duration on any real turn (LLMs are slow; if this is backwards, something is badly wrong).
3. TTFT attribute is populated on `llm_call` spans for streaming responses.
4. Per-turn log line has all four duration fields populated, in milliseconds.
5. The data answers the prioritization question: after a 10-turn real session, compute `sum(pipeline_compile_ms) / sum(total_turn_ms)`. If > 15%, step 86 (snapshot) jumps ahead of steps 84–85 (caching). If < 5%, steps 84–85 are the right next moves.

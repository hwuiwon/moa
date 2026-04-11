# Step 40 — Brain, Tool, and Pipeline Span Instrumentation

_Add OTel spans to the brain harness (session lifecycle), tool router (tool execution), and context pipeline (compilation stages)._

---

## 1. What this step is about

Step 39 instrumented the LLM providers. This step instruments everything around them — the brain harness that orchestrates turns, the tool router that executes tool calls, and the 7-stage context pipeline. After this step, a single MOA session produces a complete trace tree:

```
[session brain_turn]                          ← Root: one brain turn
├── [pipeline context_compilation]            ← Context pipeline
│   ├── [pipeline.stage identity]
│   ├── [pipeline.stage instructions]
│   ├── [pipeline.stage tools]
│   ├── [pipeline.stage skills]
│   ├── [pipeline.stage memory]
│   ├── [pipeline.stage history]
│   └── [pipeline.stage cache_optimizer]
├── [chat anthropic/claude-sonnet-4]          ← LLM call (from Step 39)
├── [execute_tool bash]                       ← Tool execution
│   └── [hand.execute local/bash]             ← Hand-level execution
├── [chat anthropic/claude-sonnet-4]          ← Second LLM call
└── [execute_tool file_write]                 ← Second tool
```

---

## 2. Files/directories to read

- **`moa-brain/src/harness.rs`** — The main brain loop. `run_brain_turn()`, `execute_tool()`, signal handling. ~1434 lines. Focus on the turn boundary and tool call dispatch.
- **`moa-brain/src/turn.rs`** — Streaming turn engine. `stream_completion_response()`.
- **`moa-brain/src/pipeline/mod.rs`** — `ContextPipeline::compile()` and the stage loop.
- **`moa-brain/src/pipeline/*.rs`** — Individual pipeline stages (identity, instructions, tools, skills, memory, history, cache).
- **`moa-hands/src/router.rs`** — `ToolRouter::execute()`, `execute_authorized()`, `execute_authorized_with_cancel()`. The tool dispatch point.
- **`moa-hands/src/local.rs`** — `LocalHandProvider::execute()`.
- **`moa-core/src/types.rs`** — `WorkingContext`, `ProcessorOutput`, `ToolOutput`.

---

## 3. Goal

Every significant operation in a MOA session is represented as an OTel span with meaningful attributes:

| Component | Span name pattern | Key attributes |
|---|---|---|
| Brain turn | `brain_turn` | `moa.session.id`, `moa.turn.number`, `moa.model` |
| Pipeline | `context_compilation` | `moa.pipeline.total_tokens`, `moa.pipeline.cache_ratio` |
| Pipeline stage | `pipeline.stage {name}` | `moa.pipeline.stage.tokens_added`, `moa.pipeline.stage.tokens_removed`, `moa.pipeline.stage.items_included` |
| Tool execution | `execute_tool {tool_name}` | `gen_ai.tool.name`, `gen_ai.tool.call.id`, `moa.tool.success`, `moa.tool.duration_ms` |
| Hand execution | `hand.execute {provider}/{tool}` | `moa.hand.provider`, `moa.hand.tier` |

---

## 4. Rules

- **Spans must nest correctly.** Brain turn is the parent. Pipeline, LLM calls, and tool executions are children. Use `tracing::Span::current()` as the parent context — as long as futures are instrumented correctly, nesting is automatic.
- **Do not change function signatures.** Add `#[instrument]` or manual span creation inside existing functions.
- **Tool input/output goes in span attributes, NOT in tracing event messages.** Tool results can be large — consider truncating output to 8KB for span attributes.
- **Error handling.** If a tool call fails, the span must have `otel.status_code = ERROR`. If a tool call is denied by policy, record that as a normal (non-error) completion with `moa.tool.denied = true`.
- **Pipeline stage duration is critical observability data.** Each stage span must accurately reflect wall-clock time for that stage.
- **Do not instrument trivially fast operations.** Don't span individual field accesses or small utility functions. Only span operations that take measurable time or represent a logical boundary.

---

## 5. Tasks

### 5a. Instrument `run_brain_turn()` in `moa-brain/src/harness.rs`

Wrap the turn in a span that carries session metadata:

```rust
let span = tracing::info_span!(
    "brain_turn",
    moa.session.id = %session_id,
    moa.turn.number = turn_number,
    moa.model = %model_name,
    moa.turn.tool_calls = tracing::field::Empty,
    moa.turn.input_tokens = tracing::field::Empty,
    moa.turn.output_tokens = tracing::field::Empty,
);
```

At the end of the turn, record aggregate stats (total tool calls, total tokens consumed in this turn).

### 5b. Instrument `ContextPipeline::compile()` in `moa-brain/src/pipeline/mod.rs`

Wrap the full compilation in a parent span, then create a child span per stage:

```rust
pub async fn compile(&self, ctx: &mut WorkingContext) -> Result<()> {
    let _span = tracing::info_span!("context_compilation").entered();
    
    for processor in &self.stages {
        let stage_span = tracing::info_span!(
            "pipeline_stage",
            otel.name = format!("pipeline.stage {}", processor.name()),
            moa.pipeline.stage.name = processor.name(),
            moa.pipeline.stage.tokens_added = tracing::field::Empty,
            moa.pipeline.stage.tokens_removed = tracing::field::Empty,
        );
        let _entered = stage_span.enter();
        
        let output = processor.process(ctx).await?;
        
        stage_span.record("moa.pipeline.stage.tokens_added", output.tokens_added as i64);
        stage_span.record("moa.pipeline.stage.tokens_removed", output.tokens_removed as i64);
    }
    
    // Record overall pipeline metrics on the parent span
    Ok(())
}
```

### 5c. Instrument `ToolRouter::execute()` in `moa-hands/src/router.rs`

Every tool execution gets a span:

```rust
pub async fn execute(&self, tool_name: &str, input: &str, ctx: &SessionContext) -> Result<ToolOutput> {
    let span = tracing::info_span!(
        "execute_tool",
        otel.name = format!("execute_tool {}", tool_name),
        gen_ai.tool.name = tool_name,
        gen_ai.tool.call.id = tracing::field::Empty,
        moa.tool.success = tracing::field::Empty,
        moa.tool.duration_ms = tracing::field::Empty,
        moa.tool.denied = false,
    );
    let _entered = span.enter();
    
    let start = std::time::Instant::now();
    let result = self.execute_inner(tool_name, input, ctx).await;
    let duration = start.elapsed();
    
    span.record("moa.tool.duration_ms", duration.as_millis() as i64);
    match &result {
        Ok(output) => span.record("moa.tool.success", output.success),
        Err(_) => span.record("moa.tool.success", false),
    }
    
    result
}
```

### 5d. Instrument hand-level execution

In `LocalHandProvider::execute()` and other `HandProvider` implementations, add a child span:

```rust
let _span = tracing::info_span!(
    "hand_execute",
    otel.name = format!("hand.execute local/{}", tool),
    moa.hand.provider = "local",
    moa.hand.tier = %tier,
).entered();
```

### 5e. Instrument approval wait time

When the brain blocks waiting for approval, the wait time should be visible:

```rust
let _span = tracing::info_span!(
    "approval_wait",
    moa.approval.tool = %tool_name,
    moa.approval.risk_level = %risk_level,
    moa.approval.decision = tracing::field::Empty,
).entered();

// ... wait for approval signal ...

span.record("moa.approval.decision", %decision);
```

---

## 6. How it should be implemented

Use `#[instrument]` for simple cases and manual `tracing::info_span!` for cases where you need to record attributes after the fact.

For **async functions**, use the `tracing::Instrument` trait:

```rust
use tracing::Instrument;

async fn my_async_fn() {
    let span = tracing::info_span!("my_operation");
    async {
        // ... work ...
    }.instrument(span).await;
}
```

For the **pipeline stages**, since each `ContextProcessor` is called in a loop, create the span in the loop body and enter it synchronously (or instrument the async call if process() is async).

For the **tool router**, the span wraps the entire dispatch — from policy check through hand execution. The LLM provider span (from Step 39) will be a sibling of tool execution spans, both children of the brain turn span.

Key: ensure the brain turn span is the active span when LLM `complete()` and tool `execute()` are called, so they automatically become children.

---

## 7. Deliverables

- [ ] `moa-brain/src/harness.rs` — Brain turn span with session/turn metadata, aggregate stats recording
- [ ] `moa-brain/src/turn.rs` — Span around streamed completion response processing (if not already covered by the LLM provider span from Step 39)
- [ ] `moa-brain/src/pipeline/mod.rs` — Parent `context_compilation` span with per-stage child spans
- [ ] `moa-hands/src/router.rs` — `execute_tool {name}` span with tool name, success, duration, denied flag
- [ ] `moa-hands/src/local.rs` — `hand.execute local/{tool}` span with provider and tier
- [ ] `moa-brain/Cargo.toml` — Add `opentelemetry.workspace = true` if needed
- [ ] `moa-hands/Cargo.toml` — Add `opentelemetry.workspace = true` if needed

---

## 8. Acceptance criteria

1. **Complete trace tree.** A single `moa exec "Create a hello world file"` produces a trace with: brain_turn → context_compilation → pipeline stages → LLM call → tool execution → hand execute → LLM call (follow-up).
2. **Pipeline stages are visible.** Each of the 7 stages appears as a separate span with `tokens_added` and `tokens_removed`.
3. **Tool execution spans show tool name and success.** `gen_ai.tool.name = "bash"`, `moa.tool.success = true`, `moa.tool.duration_ms = 1234`.
4. **Approval wait time is visible.** If a tool requires approval, the `approval_wait` span shows the decision and duration.
5. **Error propagation.** A failed tool call has `otel.status_code = ERROR` on the tool span but does NOT error the parent brain turn span (the brain handles errors).
6. **No double-spanning.** LLM calls from Step 39 nest correctly under the brain turn, not duplicated.
7. **Performance overhead < 1ms** per span creation/recording. Span creation should not measurably impact brain turn latency.
8. **All existing tests pass.** Span instrumentation is additive — nothing breaks.

---

## 9. Testing

### Unit tests

**Test 1: Pipeline stage span attributes**
```rust
#[tokio::test]
async fn pipeline_compile_emits_stage_spans() {
    let (tracer, exporter) = setup_test_tracer();
    let pipeline = ContextPipeline::new(/* test stages */);
    let mut ctx = WorkingContext::test_default();
    
    pipeline.compile(&mut ctx).await.unwrap();
    
    let spans = exporter.get_finished_spans();
    // Expect: 1 context_compilation + N stage spans
    let stage_spans: Vec<_> = spans.iter()
        .filter(|s| s.name.starts_with("pipeline.stage"))
        .collect();
    assert!(stage_spans.len() >= 7);
    
    for stage in &stage_spans {
        assert!(has_attribute(stage, "moa.pipeline.stage.tokens_added"));
    }
}
```

**Test 2: Tool execution span on success**
```rust
#[tokio::test]
async fn tool_execute_span_records_success() {
    let (tracer, exporter) = setup_test_tracer();
    let router = test_tool_router();
    
    let result = router.execute("file_read", r#"{"path":"test.txt"}"#, &ctx).await;
    
    let spans = exporter.get_finished_spans();
    let tool_span = spans.iter().find(|s| s.name == "execute_tool file_read").unwrap();
    assert_attribute(tool_span, "gen_ai.tool.name", "file_read");
    assert_attribute(tool_span, "moa.tool.success", true);
}
```

**Test 3: Tool execution span on error**
```rust
#[tokio::test]
async fn tool_execute_span_records_error() {
    // Execute a tool that will fail
    // Verify span has otel.status_code = ERROR
    // Verify moa.tool.success = false
}
```

**Test 4: Span nesting hierarchy**
```rust
#[tokio::test]
async fn brain_turn_spans_nest_correctly() {
    let (tracer, exporter) = setup_test_tracer();
    // Run a brain turn with one LLM call + one tool call
    
    let spans = exporter.get_finished_spans();
    let turn_span = find_span(&spans, "brain_turn");
    let llm_span = find_span(&spans, "chat anthropic/");
    let tool_span = find_span(&spans, "execute_tool");
    
    // LLM and tool spans should be children of the turn span
    assert_eq!(llm_span.parent_span_id, turn_span.span_id);
    assert_eq!(tool_span.parent_span_id, turn_span.span_id);
}
```

### Integration test

**Test 5: Full trace in Langfuse**
1. Run `moa exec "List files in the current directory"`
2. Open Langfuse → verify trace shows:
   - Root span (brain turn)
   - Child: context compilation with 7 sub-stages
   - Child: generation observation (LLM call)
   - Child: tool span (bash/file_search)
   - Child: second generation observation

---

## 10. Additional notes

- **Span count management.** For a typical turn with 7 pipeline stages + 1 LLM call + 1 tool call, that's ~11 spans. For a multi-turn session with 10 turns, that's ~110 spans per session. This is well within normal OTel budgets, but monitor if sessions with many tool calls (50+) create export pressure.
- **Async span context.** Rust's `tracing` requires care with async — `span.enter()` creates a guard that must not be held across `.await` points. Use `Instrument::instrument(future, span)` or `span.in_scope(|| { ... })` for sync blocks. The brain harness is highly async, so prefer `.instrument(span)`.
- **Pipeline stages may be sync or async.** Some `ContextProcessor` implementations involve I/O (memory search). If a stage does I/O, the span should cover the full duration including I/O wait. Check whether `process()` is async — if so, use `.instrument()`.
- **Compaction spans.** The compaction flow (`maybe_compact()`) should also be spanned, as it includes an LLM call for summarization. Add a `compaction` span around the compaction logic so the cost of compaction is visible separately from normal turns.

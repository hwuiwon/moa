# Step 41 — Span Context Propagation & Metadata Enrichment

_Propagate session/user/workspace context to all spans. Add Langfuse-filterable metadata. Wire up gateway-to-brain trace continuity._

---

## 1. What this step is about

Steps 39–40 instrumented individual components. This step connects them into a coherent observability story by:

1. **Propagating session/user context** so every span in a trace carries `langfuse.session.id`, `langfuse.user.id`, and workspace info — enabling per-user and per-session analytics
2. **Adding Langfuse-filterable metadata** via `langfuse.trace.metadata.*` and `langfuse.observation.metadata.*` attributes that appear in the Langfuse UI filter panel
3. **Connecting gateway → orchestrator → brain** so that a message arriving from Telegram produces a single continuous trace through the entire processing chain
4. **Adding trace-level attributes** (tags, trace name, environment) to the root span for Langfuse's trace-level views

After this step, you can open Langfuse and filter traces by user, session, workspace, model, platform, and any custom metadata dimension.

---

## 2. Files/directories to read

- **`moa-brain/src/harness.rs`** — Where the brain turn root span is created (from Step 40). This span needs session/user context.
- **`moa-orchestrator/src/local.rs`** — `LocalOrchestrator::start_session()` and `signal()`. The orchestrator spawns brain tasks — trace context must bridge from the gateway into the spawned task.
- **`moa-gateway/src/`** — Platform adapters. Incoming messages create the initial trace context.
- **`moa-core/src/types.rs`** — `SessionMeta`, `UserId`, `WorkspaceId`, `Platform`. The data sources for context attributes.
- **`moa-core/src/telemetry.rs`** — The telemetry init (from Step 38). May need a `SpanProcessor` for attribute injection.

Also reference:
- Langfuse attribute mapping: trace-level attributes like `langfuse.session.id` must be set on spans — Langfuse reads them from any span in the trace
- OTel Baggage spec: for cross-boundary context propagation

---

## 3. Goal

A trace in Langfuse for a Telegram message looks like:

```
Trace: "Fix the OAuth bug"
  langfuse.session.id = "session-abc123"
  langfuse.user.id = "user-456"
  langfuse.trace.name = "Fix the OAuth bug"
  langfuse.trace.tags = ["telegram", "workspace:webapp"]
  langfuse.environment = "production"
  langfuse.trace.metadata.workspace_id = "webapp"
  langfuse.trace.metadata.platform = "telegram"
  langfuse.trace.metadata.model = "claude-sonnet-4-20250514"
```

Every child span (LLM calls, tool executions, pipeline stages) inherits the trace context so Langfuse can:
- Show all traces for a specific user
- Group traces by session (multi-turn conversation)
- Filter by workspace, platform, or model

---

## 4. Rules

- **Set Langfuse attributes on the root span.** Langfuse extracts `langfuse.session.id`, `langfuse.user.id`, `langfuse.trace.tags`, and `langfuse.trace.metadata.*` from any span, but setting them on the root span is most reliable.
- **Use `langfuse.trace.metadata.*` for filterable dimensions.** Standard OTel attributes (like `service.name`) are NOT filterable in the Langfuse UI — only attributes under the `langfuse.trace.metadata.*` or `langfuse.observation.metadata.*` prefix.
- **Do not put sensitive data in baggage.** OTel baggage propagates across service boundaries (including to external MCP servers). Session IDs and user IDs are fine; credentials are not.
- **Trace context must survive async task spawns.** When the orchestrator spawns a brain task via `tokio::spawn`, the parent span context must be explicitly propagated into the new task.
- **One trace per brain turn, not per session.** A long-running session with 20 turns produces 20 traces. Each trace shares the same `langfuse.session.id`, which Langfuse groups into a session view. This matches MOA's event-driven architecture where each turn is a discrete unit.
- **Environment comes from config, not from the span.** The `deployment.environment` resource attribute (set in Step 38) is the authoritative environment. Additionally set `langfuse.environment` as a span attribute for Langfuse filtering.

---

## 5. Tasks

### 5a. Create a `TraceContext` struct in `moa-core`

A carrier for all the context that should be propagated into trace spans:

```rust
/// Context attributes propagated to all spans in a trace.
#[derive(Debug, Clone)]
pub struct TraceContext {
    pub session_id: SessionId,
    pub user_id: UserId,
    pub workspace_id: WorkspaceId,
    pub platform: Option<Platform>,
    pub model: String,
    pub trace_name: Option<String>,  // First line of user message
    pub tags: Vec<String>,
    pub environment: Option<String>,
}

impl TraceContext {
    /// Sets Langfuse-recognized attributes on a tracing span.
    pub fn apply_to_span(&self, span: &tracing::Span) {
        span.record("langfuse.session.id", self.session_id.to_string());
        span.record("langfuse.user.id", self.user_id.to_string());
        // ... etc
    }
}
```

### 5b. Enrich the brain turn root span (harness.rs)

Update the brain turn span from Step 40 to include all trace-level Langfuse attributes:

```rust
let span = tracing::info_span!(
    "brain_turn",
    // OTel standard
    otel.name = trace_name,
    
    // Langfuse trace-level
    langfuse.session.id = %session_id,
    langfuse.user.id = %user_id,
    langfuse.trace.name = %trace_name,
    langfuse.environment = tracing::field::Empty,
    
    // Langfuse filterable metadata
    langfuse.trace.metadata.workspace_id = %workspace_id,
    langfuse.trace.metadata.platform = tracing::field::Empty,
    langfuse.trace.metadata.model = %model,
    langfuse.trace.metadata.turn_number = turn_number,
    
    // MOA-specific
    moa.session.id = %session_id,
    moa.turn.number = turn_number,
);
```

Set `langfuse.trace.tags` as a JSON array string: `["telegram", "workspace:webapp", "v2.1"]`.

### 5c. Propagate trace context through `tokio::spawn`

In `LocalOrchestrator::start_session()`, when spawning the brain task:

```rust
use tracing::Instrument;

let span = tracing::info_span!(
    "session",
    langfuse.session.id = %session_id,
    langfuse.user.id = %user_id,
);

let task = tokio::spawn(
    async move {
        brain_loop(session_id, store, memory, llm, hands, signal_rx, event_tx).await
    }
    .instrument(span)
);
```

This ensures the brain loop and all its child spans (turns, LLM calls, tools) are children of the session span.

### 5d. Add Langfuse metadata to observation spans

On tool execution spans, add `langfuse.observation.metadata.*` for filterable tool-level dimensions:

```rust
// In tool router execute()
span.record("langfuse.observation.metadata.tool_category", category);
span.record("langfuse.observation.metadata.sandbox_tier", tier);
span.record("langfuse.observation.metadata.approval_required", requires_approval);
```

On LLM generation spans (update Step 39's instrumentation):
```rust
span.record("langfuse.observation.metadata.cache_hit_ratio", cache_ratio);
span.record("langfuse.observation.metadata.context_tokens", context_size);
```

### 5e. Set trace name from user message

The trace name should be the first user message (truncated to 200 chars, per Langfuse constraints):

```rust
fn trace_name_from_message(msg: &str) -> String {
    let first_line = msg.lines().next().unwrap_or(msg);
    if first_line.len() > 200 {
        format!("{}...", &first_line[..197])
    } else {
        first_line.to_string()
    }
}
```

### 5f. Wire gateway → orchestrator trace continuity

When a message arrives from a platform adapter, create a root span and propagate it:

```rust
// In gateway message handler
let span = tracing::info_span!(
    "gateway_receive",
    langfuse.trace.metadata.platform = %platform,
    langfuse.trace.metadata.channel = %channel_ref,
);
let _entered = span.enter();

// Signal the orchestrator — the span context is now active
orchestrator.signal(session_id, SessionSignal::QueueMessage(msg)).await?;
```

---

## 6. How it should be implemented

The key challenge is passing `TraceContext` through the system without changing every function signature. Two approaches:

**Option A: Thread through parameters.** Add `TraceContext` to `run_brain_turn()` params. Clean but requires signature changes.

**Option B: Extract from session metadata.** The brain already loads `SessionMeta` at the start of each turn. Construct `TraceContext` from `SessionMeta` fields. No signature changes needed.

**Recommended: Option B.** The brain harness already has access to `SessionMeta` (which contains `user_id`, `workspace_id`, `model`, `platform`). Build `TraceContext` from that and apply it to the turn span.

For tag generation, derive tags from context:
```rust
fn generate_trace_tags(ctx: &TraceContext) -> Vec<String> {
    let mut tags = vec![];
    if let Some(platform) = &ctx.platform {
        tags.push(platform.to_string());
    }
    tags.push(format!("workspace:{}", ctx.workspace_id));
    tags
}
```

---

## 7. Deliverables

- [ ] `moa-core/src/types.rs` — `TraceContext` struct with `apply_to_span()` method
- [ ] `moa-brain/src/harness.rs` — Brain turn root span enriched with all Langfuse trace/metadata attributes
- [ ] `moa-orchestrator/src/local.rs` — Session span wrapping the brain task spawn, propagating context via `.instrument()`
- [ ] `moa-hands/src/router.rs` — `langfuse.observation.metadata.*` attributes on tool execution spans
- [ ] `moa-gateway/src/` — Root span creation when messages arrive from platform adapters
- [ ] `moa-core/src/telemetry.rs` — Optional: helper to extract `TraceContext` from `SessionMeta`

---

## 8. Acceptance criteria

1. **Langfuse session grouping works.** Multiple turns in the same session appear grouped under one session in Langfuse's session view.
2. **User filtering works.** Langfuse's user filter shows traces per user, with per-user cost/token aggregation.
3. **Metadata is filterable.** In Langfuse, you can filter traces by `workspace_id`, `platform`, and `model` using the metadata filter panel.
4. **Trace names are human-readable.** Each trace is named after the user's message (truncated to 200 chars).
5. **Tags appear in Langfuse.** Tags like `["telegram", "workspace:webapp"]` are visible and filterable.
6. **Cross-async-task continuity.** Spans from within `tokio::spawn`'d brain tasks correctly parent under the session/gateway span.
7. **No sensitive data in span attributes.** User IDs and session IDs are OK. No API keys, tokens, or message content in metadata attributes (content goes only in `langfuse.observation.input/output`, if configured).
8. **All existing tests pass.**

---

## 9. Testing

### Unit tests

**Test 1: TraceContext construction from SessionMeta**
```rust
#[test]
fn trace_context_from_session_meta() {
    let meta = SessionMeta {
        user_id: UserId::new(),
        workspace_id: WorkspaceId::new("webapp"),
        platform: Some(Platform::Telegram),
        model: "claude-sonnet-4-20250514".into(),
        ..Default::default()
    };
    let ctx = TraceContext::from_session_meta(&meta, "Fix OAuth bug");
    assert_eq!(ctx.trace_name.as_deref(), Some("Fix OAuth bug"));
    assert!(ctx.tags.contains(&"telegram".to_string()));
}
```

**Test 2: Trace name truncation**
```rust
#[test]
fn trace_name_truncates_at_200_chars() {
    let long_msg = "a".repeat(300);
    let name = trace_name_from_message(&long_msg);
    assert!(name.len() <= 200);
    assert!(name.ends_with("..."));
}
```

**Test 3: Tags generation**
```rust
#[test]
fn tags_include_platform_and_workspace() {
    let ctx = TraceContext {
        platform: Some(Platform::Slack),
        workspace_id: WorkspaceId::new("myproject"),
        ..test_defaults()
    };
    let tags = generate_trace_tags(&ctx);
    assert!(tags.contains(&"slack".to_string()));
    assert!(tags.contains(&"workspace:myproject".to_string()));
}
```

### Integration tests

**Test 4: Session grouping in Langfuse**
1. Run two turns in the same session:
   ```bash
   moa exec "What is Rust?" 
   moa resume <session-id>
   # then ask "Tell me more about ownership"
   ```
2. In Langfuse → Sessions → verify both traces appear under the same session, with the session ID matching MOA's session ID.

**Test 5: Metadata filter in Langfuse**
1. Run `moa exec "hello"` with workspace configured
2. In Langfuse → Traces → Filters → verify `workspace_id` and `model` appear as filterable metadata dimensions.

**Test 6: Async spawn context propagation**
```rust
#[tokio::test]
async fn spawned_brain_task_inherits_trace_context() {
    let (tracer, exporter) = setup_test_tracer();
    
    let session_span = tracing::info_span!("session", langfuse.session.id = "test-session");
    
    let handle = tokio::spawn(
        async {
            let _turn = tracing::info_span!("brain_turn").entered();
            // Simulate work
        }
        .instrument(session_span)
    );
    handle.await.unwrap();
    
    let spans = exporter.get_finished_spans();
    let turn = find_span(&spans, "brain_turn");
    let session = find_span(&spans, "session");
    assert_eq!(turn.parent_span_id, session.span_id);
}
```

---

## 10. Additional notes

- **Langfuse attribute length limits.** `sessionId` and `userId` must be US-ASCII, max 200 chars. `tags` max 200 chars each. `environment` max 40 chars, regex `^(?!langfuse)[a-z0-9-_]+$`. Validate in `TraceContext` construction.
- **Langfuse environment vs OTel deployment.environment.** Set BOTH: `deployment.environment` as a resource attribute (for Grafana/Tempo) and `langfuse.environment` as a span attribute (for Langfuse filtering). They should have the same value.
- **Gateway span timing.** The gateway span covers the time from message receipt to orchestrator dispatch — typically very fast (< 1ms). Don't over-instrument gateway internals.
- **Multi-brain sessions.** If sub-brains are spawned (child workflows), each sub-brain turn should be its own trace but share the same `langfuse.session.id`. This preserves the session grouping while keeping individual turn traces manageable.
- **Future: distributed tracing.** When MOA runs in cloud mode with separate gateway and brain processes, OTel trace context propagation (via W3C TraceContext headers) will connect spans across processes. The span structure from this step is designed to work with that future architecture.

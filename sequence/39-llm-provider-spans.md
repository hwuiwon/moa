# Step 39 — LLM Provider Span Instrumentation

_Add GenAI semantic convention spans to every LLM completion call across all providers._

---

## 1. What this step is about

Every LLM completion call in MOA should emit an OpenTelemetry span carrying the full set of GenAI semantic convention attributes — model name, token counts, cost, latency, time-to-first-token, temperature, and input/output content. These spans are what Langfuse classifies as **generation** observations (any span with `gen_ai.request.model` is auto-detected as a generation).

This step instruments `moa-providers` — the Anthropic, OpenAI, and OpenRouter provider implementations — without changing their external API. The brain harness and tool router remain untouched until Step 40.

---

## 2. Files/directories to read

- **`moa-providers/src/anthropic.rs`** — The primary provider. `complete()` method (~line 103). Understand how streaming works, where `CompletionStream` is constructed, where token counts are captured.
- **`moa-providers/src/openai.rs`** — OpenAI provider. Same `complete()` pattern.
- **`moa-providers/src/openrouter.rs`** — OpenRouter provider. Delegates to the OpenAI common path.
- **`moa-providers/src/common.rs`** — Shared streaming/parsing logic. `StreamState` struct, token counting, `CompletionContent` processing.
- **`moa-core/src/types.rs`** — `CompletionRequest`, `CompletionStream`, `ModelCapabilities`, `TokenPricing`. Understand what data is available.
- **`moa-core/src/traits.rs`** — `LLMProvider` trait, `complete()` signature.
- **`moa-providers/Cargo.toml`** — Current dependencies (has `tracing` but not `opentelemetry` directly).

Also reference:
- OTel GenAI Semantic Conventions for spans: `https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-spans/`
- Langfuse OTel attribute mapping: `https://langfuse.com/integrations/native/opentelemetry`

---

## 3. Goal

After this step, every LLM API call produces a span like:

```
Span: "chat anthropic/claude-sonnet-4-20250514"
  gen_ai.system = "anthropic"
  gen_ai.operation.name = "chat"
  gen_ai.request.model = "claude-sonnet-4-20250514"
  gen_ai.response.model = "claude-sonnet-4-20250514"
  gen_ai.request.temperature = 0.7
  gen_ai.request.max_tokens = 8192
  gen_ai.usage.prompt_tokens = 1500
  gen_ai.usage.completion_tokens = 400
  gen_ai.usage.total_tokens = 1900
  gen_ai.usage.cost = 0.0057
  langfuse.observation.completion_start_time = "2026-04-10T14:30:01.234Z"
  langfuse.observation.input = "{...}"
  langfuse.observation.output = "{...}"
  duration: 2.3s
```

Visible in both Langfuse (as a `generation` observation) and Grafana Tempo (as a standard span with attributes).

---

## 4. Rules

- **Do not change the `LLMProvider` trait signature.** Span creation happens inside provider implementations, not at the trait level.
- **Do not log full prompt/response content at INFO level.** Input/output go into span attributes (which OTel exports) but NOT into `tracing::info!` calls (which go to console/file logs). Use span attributes for structured data, tracing events for human-readable summaries.
- **Use `tracing` spans, not raw `opentelemetry` API.** The `tracing-opentelemetry` bridge converts `tracing::Span` into OTel spans. Set OTel attributes via `span.set_attribute()` or `tracing::Span::current().record()`.
- **Do not set non-standard `gen_ai.usage.*` sub-attributes.** Langfuse has a known bug (issue #6024) where non-standard usage sub-keys cause spans to vanish. Stick to `prompt_tokens`, `completion_tokens`, `total_tokens`, and `cost` only.
- **Token counts and cost must be numeric, not strings.** Type coercion issues are a common OTel pitfall.
- **Time-to-first-token must be captured accurately** — record the wall-clock time when the first content chunk arrives during streaming, not when the response object is created.
- **Span names follow the convention:** `"{operation} {system}/{model}"` — e.g., `"chat anthropic/claude-sonnet-4-20250514"`.

---

## 5. Tasks

### 5a. Add `opentelemetry` dependency to `moa-providers`

In `moa-providers/Cargo.toml`:
```toml
opentelemetry.workspace = true
tracing.workspace = true
```

### 5b. Create a shared instrumentation helper module

Create `moa-providers/src/instrumentation.rs` with helpers used by all three providers:

```rust
use opentelemetry::KeyValue;
use tracing::Span;
use chrono::{DateTime, Utc};

/// Sets GenAI semantic convention attributes on the current span.
pub struct LLMSpanAttributes {
    pub system: &'static str,        // "anthropic", "openai", "openrouter"
    pub operation: &'static str,     // "chat"
    pub request_model: String,
    pub response_model: Option<String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<usize>,
    pub input_tokens: Option<usize>,
    pub output_tokens: Option<usize>,
    pub total_tokens: Option<usize>,
    pub cost: Option<f64>,           // in dollars
    pub completion_start_time: Option<DateTime<Utc>>,
    pub input_content: Option<String>,  // JSON serialized
    pub output_content: Option<String>, // JSON serialized
}

/// Record all attributes on the current tracing span.
/// Call this after the LLM response is fully received.
pub fn record_llm_span_attributes(span: &Span, attrs: &LLMSpanAttributes) {
    // ... set each attribute via span.record() or 
    // opentelemetry context
}

/// Build the span name: "{operation} {system}/{model}"
pub fn llm_span_name(operation: &str, system: &str, model: &str) -> String {
    format!("{} {}/{}", operation, system, model)
}
```

### 5c. Instrument `AnthropicProvider::complete()`

Wrap the entire `complete()` method body in a tracing span. The tricky part: most attributes (token counts, cost) are only known after streaming completes, so you need to:

1. Create the span at the start with known attributes (system, model, temperature)
2. Record TTFT when the first `ContentBlockDelta` arrives in the stream
3. Record token counts and cost from the `message_stop` / `message_delta` event that carries usage data
4. Close the span when the stream finishes

The recommended approach: wrap the streaming `CompletionStream` in a span-aware adapter that records attributes as they become available, then finalizes the span when the stream completes or errors.

### 5d. Instrument `OpenAIProvider::complete()` and `OpenRouterProvider::complete()`

Same pattern. OpenRouter uses `system = "openrouter"` and the underlying model goes in `gen_ai.request.model`.

### 5e. Capture time-to-first-token accurately

In the streaming response processor, record the timestamp when the first non-empty content chunk arrives:

```rust
if first_token_time.is_none() && !chunk.is_empty() {
    first_token_time = Some(Utc::now());
}
```

Then set `langfuse.observation.completion_start_time` as an ISO 8601 string attribute.

### 5f. Calculate and record cost

Use the existing `TokenPricing` from `ModelCapabilities` to compute cost:

```rust
let cost = (input_tokens as f64 * pricing.input_per_mtok / 1_000_000.0)
    + (output_tokens as f64 * pricing.output_per_mtok / 1_000_000.0);
// Set as gen_ai.usage.cost
```

---

## 6. How it should be implemented

The cleanest pattern for streaming instrumentation is a **span-completing wrapper**. Rather than modifying the internal stream processing, wrap the returned `CompletionStream` so that span finalization happens when the consumer drains the stream.

Pseudocode for the Anthropic provider:

```rust
async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
    let model = request.model.as_deref().unwrap_or(&self.default_model);
    let span_name = llm_span_name("chat", "anthropic", model);
    
    // Create span with upfront attributes
    let span = tracing::info_span!(
        "llm_completion",
        otel.name = %span_name,
        gen_ai.system = "anthropic",
        gen_ai.operation.name = "chat",
        gen_ai.request.model = %model,
        gen_ai.request.temperature = tracing::field::Empty,
        gen_ai.request.max_tokens = tracing::field::Empty,
        gen_ai.usage.prompt_tokens = tracing::field::Empty,
        gen_ai.usage.completion_tokens = tracing::field::Empty,
        gen_ai.usage.total_tokens = tracing::field::Empty,
        gen_ai.usage.cost = tracing::field::Empty,
    );
    
    if let Some(temp) = request.temperature {
        span.record("gen_ai.request.temperature", temp);
    }
    
    // Build and send the HTTP request (existing code)
    let (content_tx, content_rx) = mpsc::channel(64);
    let stream = self.build_stream(request, content_tx, span.clone());
    
    // The stream task fills in span attributes as chunks arrive
    // and closes the span when done
    
    Ok(CompletionStream { receiver: content_rx })
}
```

In the stream processing task, after parsing the final `message_delta` with usage:

```rust
span.record("gen_ai.usage.prompt_tokens", usage.input_tokens as i64);
span.record("gen_ai.usage.completion_tokens", usage.output_tokens as i64);
span.record("gen_ai.usage.total_tokens", 
    (usage.input_tokens + usage.output_tokens) as i64);
span.record("gen_ai.usage.cost", cost);
```

**Important:** `tracing::field::Empty` reserves the field for later recording. Only pre-declared fields can be recorded after span creation.

---

## 7. Deliverables

- [ ] `moa-providers/src/instrumentation.rs` — New module with `LLMSpanAttributes`, `record_llm_span_attributes()`, `llm_span_name()`, and cost calculation helpers
- [ ] `moa-providers/src/anthropic.rs` — `complete()` instrumented with GenAI spans, TTFT capture, and cost recording
- [ ] `moa-providers/src/openai.rs` — Same instrumentation
- [ ] `moa-providers/src/openrouter.rs` — Same instrumentation (with `system = "openrouter"`)
- [ ] `moa-providers/src/lib.rs` — `mod instrumentation;` added
- [ ] `moa-providers/Cargo.toml` — `opentelemetry.workspace = true` added

---

## 8. Acceptance criteria

1. **Every `complete()` call produces a span** with `gen_ai.system`, `gen_ai.request.model`, and `gen_ai.operation.name = "chat"`.
2. **Token counts are present** on completed spans: `gen_ai.usage.prompt_tokens`, `gen_ai.usage.completion_tokens`, `gen_ai.usage.total_tokens`.
3. **Cost is present** as `gen_ai.usage.cost` in dollars (float).
4. **TTFT is present** as `langfuse.observation.completion_start_time` (ISO 8601 string).
5. **Span name follows convention**: `"chat anthropic/claude-sonnet-4-20250514"`.
6. **Error spans**: If the LLM call fails, the span has `otel.status_code = ERROR` and `otel.status_description` with the error message.
7. **No regressions**: All existing tests pass. The `CompletionStream` API is unchanged.
8. **Spans are children of the current context**: If a parent span exists (from the brain harness, added in Step 40), the LLM span nests under it.

---

## 9. Testing

### Unit tests (in `moa-providers/tests/` or `moa-providers/src/instrumentation.rs`)

**Test 1: Span name construction**
```rust
#[test]
fn llm_span_name_format() {
    assert_eq!(
        llm_span_name("chat", "anthropic", "claude-sonnet-4-20250514"),
        "chat anthropic/claude-sonnet-4-20250514"
    );
}
```

**Test 2: Cost calculation**
```rust
#[test]
fn cost_calculation_correct() {
    let pricing = TokenPricing {
        input_per_mtok: 3.0,   // $3/MTok
        output_per_mtok: 15.0,  // $15/MTok
        cached_input_per_mtok: Some(0.30),
    };
    let cost = calculate_cost(1000, 500, &pricing);
    // 1000 * 3.0 / 1M + 500 * 15.0 / 1M = 0.003 + 0.0075 = 0.0105
    assert!((cost - 0.0105).abs() < 1e-10);
}
```

**Test 3: TTFT capture**
```rust
#[test]
fn ttft_captured_on_first_content_chunk() {
    let mut ttft: Option<DateTime<Utc>> = None;
    let chunks = vec!["", "", "Hello", " world"];
    
    for chunk in chunks {
        if ttft.is_none() && !chunk.is_empty() {
            ttft = Some(Utc::now());
        }
    }
    
    assert!(ttft.is_some());
}
```

### Integration tests (with mock or real provider)

**Test 4: Span attributes end-to-end (mock provider)**

Use a test `TracerProvider` with an in-memory exporter to capture spans:

```rust
#[tokio::test]
async fn anthropic_complete_emits_genai_span() {
    let (provider, exporter) = setup_test_tracer();
    // ... create AnthropicProvider with mock HTTP responses ...
    
    let request = CompletionRequest { /* ... */ };
    let stream = provider.complete(request).await.unwrap();
    // Drain the stream
    drain_stream(stream).await;
    
    // Force flush
    provider.force_flush().unwrap();
    
    let spans = exporter.get_finished_spans();
    assert_eq!(spans.len(), 1);
    
    let span = &spans[0];
    assert!(span.name.starts_with("chat anthropic/"));
    assert_attribute(span, "gen_ai.system", "anthropic");
    assert_attribute(span, "gen_ai.usage.prompt_tokens", /* expected */);
    assert_attribute(span, "gen_ai.usage.completion_tokens", /* expected */);
}
```

**Test 5: Error span on provider failure**
```rust
#[tokio::test]
async fn anthropic_complete_error_sets_span_error_status() {
    // Mock a 500 response from Anthropic
    // Verify span has otel.status_code = ERROR
}
```

### Manual validation

**Test 6: Langfuse generation observation**
1. Configure MOA to export to Langfuse (via Step 38's config)
2. Run `moa exec "What is 2+2?"`
3. Open Langfuse UI → Traces → verify:
   - A trace exists with a `generation` observation
   - Model name, token counts, cost, and latency are populated
   - TTFT (time to first token) is visible in the observation detail

---

## 10. Additional notes

- **`tracing` vs raw `opentelemetry`**: The `tracing-opentelemetry` bridge maps `tracing::Span` fields to OTel span attributes. Field names with dots (like `gen_ai.system`) work correctly — they become the OTel attribute key as-is. However, you may need to use the `opentelemetry` API directly for setting attributes after span creation if `tracing::Span::record()` doesn't support the dotted field names in your version. Test this early.
- **Alternative approach**: If `tracing::field::Empty` + `record()` proves awkward for dotted field names, consider using `Span::current()` from `tracing` combined with `span.set_attribute()` from `opentelemetry::trace::Span` (via the `tracing-opentelemetry` extension trait). This gives full control over attribute names.
- **Streaming lifecycle**: The span should stay open for the entire duration of streaming. If MOA spawns a background task for stream processing, the span must be entered in that task's context. Use `span.in_scope(|| { ... })` or `Instrument::instrument(future, span)`.
- **Input/output content**: For now, set `langfuse.observation.input` and `langfuse.observation.output` as JSON strings containing the messages array and response text. These can be large — consider truncating to a reasonable limit (e.g., 32KB) or making content capture configurable. In production, you may want to disable content capture for privacy.

# Step 76 — Explicit Cache Control Breakpoints for Anthropic Provider

_Wire the pipeline's cache breakpoints through to Anthropic's `cache_control` API parameter. The pipeline already identifies the stable prefix boundary — but the provider currently ignores it, missing 80%+ potential cache hit rate improvement._

---

## 1. What this step is about

MOA's context pipeline already has the right structure for KV-cache optimization: stages 1–4 (identity, instructions, tools, skills) produce a stable prefix, and `CacheOptimizer` (stage 7) measures the cache ratio. But this information is never wired through to the Anthropic API request. The `cache_control: { type: "ephemeral" }` annotation on message blocks is what triggers Anthropic's prompt caching — without it, caching is opportunistic at best.

ProjectDiscovery demonstrated that explicitly placing cache breakpoints improved their cache hit rate from **7% to 84%**, cutting costs 59%. The PwC benchmark found prompt caching reduced API costs **41–80%** across 500+ agent sessions.

Anthropic's caching model:
- Cache reads cost **0.1x** base input price (90% discount)
- 5-minute TTL writes cost **1.25x** base price (break even after 1 cache hit)
- Maximum **4 explicit breakpoints** per request
- Minimum **1,024 tokens** per cached block (Sonnet), **2,048** for Haiku
- Cache key is a **byte-level prefix hash** — any change before a breakpoint invalidates everything downstream

---

## 2. Files to read

- **`moa-brain/src/pipeline/cache.rs`** — `CacheOptimizer` stage. Already computes `cache_prefix_ratio` and measures cache efficiency. Does NOT emit `cache_control` annotations.
- **`moa-brain/src/pipeline/mod.rs`** — `cache_prefix_ratio()` helper. `WorkingContext.cache_breakpoints` stores the breakpoint indices.
- **`moa-core/src/types/context.rs`** (or wherever `WorkingContext`, `ContextMessage`, `CompletionRequest` live) — These types need to carry cache breakpoint information through to the provider.
- **`moa-providers/src/anthropic.rs`** — The Anthropic provider. Must annotate the final message block before each cache breakpoint with `cache_control: { type: "ephemeral" }`.
- **`moa-brain/src/pipeline/skills.rs`** — `SkillInjector` calls `ctx.mark_cache_breakpoint()` at the end of stage 4. This is the primary breakpoint.
- **`moa-core/src/types/completion.rs`** (or similar) — `CompletionRequest` struct. May need a field for cache breakpoints.

---

## 3. Goal

After this step:
1. The `WorkingContext.cache_breakpoints` indices are forwarded to `CompletionRequest`
2. The Anthropic provider annotates the appropriate message/system blocks with `cache_control`
3. Cache hit rates are measurable via the response's `cache_creation_input_tokens` and `cache_read_input_tokens` fields
4. The CacheOptimizer logs actual cache hit rates from provider responses
5. Other providers (OpenAI, Gemini) ignore the breakpoints harmlessly

---

## 4. Rules

- **Maximum 4 breakpoints.** If `cache_breakpoints` has more than 4 entries, use only the last 4 (closest to the dynamic content boundary). Anthropic's API rejects >4.
- **Minimum token threshold.** Don't place a breakpoint if the content before it is under 1,024 tokens. This avoids wasting a breakpoint slot on a block too small to cache.
- **Breakpoint placement:** Annotate the `cache_control` field on the **last content block** of the message at each breakpoint index. For system messages, this is the system prompt block. For tools, this is the last tool definition.
- **Tool definitions get a breakpoint.** Since tool schemas are part of the stable prefix (they don't change within a session), place a breakpoint on the last tool definition. This is the most impactful single breakpoint.
- **Don't break the prefix.** The pipeline's `sort_json_keys` in `CacheOptimizer` ensures deterministic serialization. The provider must also ensure deterministic serialization of tool schemas.
- **Log cache metrics.** When the Anthropic response includes `cache_creation_input_tokens` and `cache_read_input_tokens`, log them as structured fields on the LLM span.

---

## 5. Tasks

### 5a. Add cache breakpoints to `CompletionRequest`

```rust
pub struct CompletionRequest {
    // ... existing fields
    /// Message indices where cache breakpoints should be placed.
    /// Provider-specific: Anthropic uses these for `cache_control` annotations.
    pub cache_breakpoints: Vec<usize>,
}
```

### 5b. Propagate breakpoints from `WorkingContext` to `CompletionRequest`

In `WorkingContext::into_request()` (or wherever the conversion happens):

```rust
pub fn into_request(self) -> CompletionRequest {
    CompletionRequest {
        // ... existing fields
        cache_breakpoints: self.cache_breakpoints,
    }
}
```

### 5c. Annotate in the Anthropic provider

In `anthropic.rs`, when building the API request body, apply `cache_control` annotations:

```rust
fn apply_cache_breakpoints(
    messages: &mut Vec<serde_json::Value>,
    system: &mut Option<serde_json::Value>,
    tools: &mut Option<Vec<serde_json::Value>>,
    breakpoints: &[usize],
) {
    // Limit to 4 breakpoints
    let breakpoints: Vec<_> = breakpoints.iter().rev().take(4).rev().copied().collect();

    // Always cache tools if present (they're the most stable prefix element)
    if let Some(tools) = tools {
        if let Some(last_tool) = tools.last_mut() {
            last_tool["cache_control"] = serde_json::json!({"type": "ephemeral"});
        }
    }

    // Cache system prompt blocks
    if let Some(system) = system {
        if let Some(blocks) = system.as_array_mut() {
            if let Some(last_block) = blocks.last_mut() {
                last_block["cache_control"] = serde_json::json!({"type": "ephemeral"});
            }
        }
    }

    // Apply remaining breakpoints to message boundaries
    for &bp_idx in &breakpoints {
        if bp_idx > 0 && bp_idx <= messages.len() {
            let msg = &mut messages[bp_idx - 1];
            if let Some(content) = msg.get_mut("content") {
                if let Some(blocks) = content.as_array_mut() {
                    if let Some(last_block) = blocks.last_mut() {
                        last_block["cache_control"] = serde_json::json!({"type": "ephemeral"});
                    }
                }
            }
        }
    }
}
```

### 5d. Parse cache metrics from Anthropic response

The Anthropic response includes `usage.cache_creation_input_tokens` and `usage.cache_read_input_tokens`. Parse these and include in `CompletionResponse`:

```rust
pub struct CompletionResponse {
    // ... existing fields
    pub cached_input_tokens: usize,  // already exists
    // Ensure this is populated from cache_read_input_tokens
}
```

### 5e. Log cache metrics

In the LLM provider span (from Step 39), record:

```rust
span.record("gen_ai.usage.cache_read_tokens", cached_input_tokens as i64);
span.record("gen_ai.usage.cache_creation_tokens", cache_creation_tokens as i64);

let cache_hit_rate = if input_tokens > 0 {
    cached_input_tokens as f64 / input_tokens as f64
} else {
    0.0
};
span.record("moa.cache.hit_rate", cache_hit_rate);
tracing::info!(
    cache_read = cached_input_tokens,
    cache_creation = cache_creation_tokens,
    cache_hit_rate = format!("{:.1}%", cache_hit_rate * 100.0),
    "Anthropic cache metrics"
);
```

### 5f. Other providers ignore breakpoints

In `openai.rs` and `gemini.rs`, simply ignore `request.cache_breakpoints`. OpenAI's automatic caching doesn't use explicit annotations.

### 5g. Add tests

```rust
#[test]
fn cache_breakpoints_propagate_to_completion_request() {
    let mut ctx = WorkingContext::new(&session, capabilities);
    ctx.append_system("identity prompt".to_string());
    ctx.append_system("tool definitions".to_string());
    ctx.mark_cache_breakpoint();
    ctx.append_system("dynamic memory".to_string());

    let request = ctx.into_request();
    assert_eq!(request.cache_breakpoints, vec![2]); // breakpoint after 2nd message
}

#[test]
fn anthropic_annotates_cache_control_on_tool_definitions() {
    // Integration test: build a request with tools and breakpoints,
    // verify the serialized JSON contains cache_control on the last tool
}
```

---

## 6. Deliverables

- [ ] `moa-core/src/types/completion.rs` — `cache_breakpoints` field on `CompletionRequest`
- [ ] `moa-brain/src/pipeline/mod.rs` or context conversion — Propagate breakpoints
- [ ] `moa-providers/src/anthropic.rs` — `cache_control` annotation logic, cache metric parsing
- [ ] `moa-providers/src/instrumentation.rs` — Cache hit rate logging on spans
- [ ] OpenAI/Gemini providers ignore breakpoints without error
- [ ] Tests

---

## 7. Acceptance criteria

1. Anthropic API requests include `cache_control: {"type": "ephemeral"}` on the last tool definition and system prompt block.
2. Cache metrics (`cache_read_input_tokens`, `cache_creation_input_tokens`) appear in tracing spans.
3. On the second turn of a session, `cache_read_input_tokens > 0` (confirming the cache is being hit).
4. OpenAI and Gemini providers work without error when `cache_breakpoints` is populated.
5. Maximum 4 breakpoints are sent to the API, regardless of how many the pipeline marks.
6. `cargo test -p moa-providers -p moa-brain` passes.

# Step 79 — Cache Hit Rate Baseline Instrumentation

_Parse Anthropic's cache usage fields out of every completion response, expose them as first-class fields on `TokenUsage`, emit them as OTel span attributes, and aggregate a per-session and per-turn cache hit rate metric. This number is the single most important diagnostic for whether step 76 actually works and for whether steps 83–85 are worth doing._

---

## 1. What this step is about

Step 76 wired `cache_control: {"type": "ephemeral"}` breakpoints into the Anthropic provider. We don't know whether they're hitting. Anthropic returns the answer in every response under `usage.cache_creation_input_tokens` and `usage.cache_read_input_tokens`, but today we throw that away.

Without this instrumentation:
- We cannot measure whether step 76 delivered the expected ~70% hit rate.
- We cannot compare before/after for steps 83 (static prefix relocation) and 84 (conversation sliding window).
- We cannot diagnose cache invalidation regressions when a future commit accidentally adds dynamic content to the system prompt.

ProjectDiscovery's journey from 7% → 84% cache hit rate is the canonical reference case. That only became possible because they were measuring. We need the same.

---

## 2. Files to read

- `moa-core/src/types/provider.rs` (or wherever `TokenUsage` lives) — the existing token accounting struct.
- `moa-providers/src/anthropic.rs` — where completion responses are parsed.
- `moa-providers/src/openai.rs` — OpenAI also reports cached tokens under `usage.prompt_tokens_details.cached_tokens`.
- `moa-providers/src/gemini.rs` — Gemini uses `usage_metadata.cached_content_token_count`.
- `moa-core/src/types/event.rs` — `Event::BrainResponse` carries per-turn token counts. Schema must grow.
- `moa-brain/src/pipeline/llm_provider_spans.rs` (or wherever step 39 wired the OTel span) — extend with new attributes.
- `moa-core/src/config.rs` — confirm metric export config exists from steps 38–41.

---

## 3. Goal

1. Every completion response from every provider populates four fields on `TokenUsage`:
   - `input_tokens_uncached: u32`
   - `input_tokens_cache_write: u32`   (tokens that paid the 1.25× premium to be cached)
   - `input_tokens_cache_read: u32`    (tokens that paid 0.1× to read from cache)
   - `output_tokens: u32`
2. `Event::BrainResponse` carries these four fields. Old events (pre-step-79) continue to deserialize via serde defaults.
3. The OTel span around `LLMProvider::complete` has span attributes: `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, `gen_ai.usage.cache_read_tokens`, `gen_ai.usage.cache_write_tokens`.
4. A per-turn structured log line prints `cache_hit_rate = cache_read / (cache_read + uncached + cache_write)`.
5. A running session-level tally tracks cumulative cache hit rate, exposed via `SessionMeta` (add a column or a method).

---

## 4. Rules

- **Add fields, don't replace.** The existing `input_tokens` / `output_tokens` shape must keep working for consumers that haven't been updated. Define `input_tokens` as a computed accessor: `input_tokens_uncached + input_tokens_cache_write + input_tokens_cache_read`.
- **Use serde defaults for new fields on events.** Old persisted events must still deserialize. Put `#[serde(default)]` on the new fields in `Event::BrainResponse`.
- **Default to zero, not None.** These fields are counters. A provider that doesn't report them (or reports zero) is semantically equivalent to "no cache activity." Avoid `Option<u32>` to keep aggregation trivial.
- **Do not change cost computation in this step.** Cost tracking landed in step 65. Wire the new fields into cost computation as a separate concern (step 93 — session-level cost tracking). This step is instrumentation-only.
- **Name metrics with OTel GenAI semantic conventions.** `gen_ai.usage.input_tokens` and `gen_ai.usage.output_tokens` are spec'd. Cache-specific attributes are not standardized; use `gen_ai.usage.cache_read_tokens` / `gen_ai.usage.cache_write_tokens` and document the convention in `moa-core/src/observability.rs` or equivalent.
- **Hit rate is a derived metric, not a stored column.** Store the four counters. Compute the ratio in log/metric sites. That way any consumer can compute the ratio at any aggregation level (per turn, per session, per workspace, per hour).

---

## 5. Tasks

### 5a. Extend `TokenUsage`

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    #[serde(default)]
    pub input_tokens_uncached: u32,
    #[serde(default)]
    pub input_tokens_cache_write: u32,
    #[serde(default)]
    pub input_tokens_cache_read: u32,
    pub output_tokens: u32,
}

impl TokenUsage {
    pub fn total_input_tokens(&self) -> u32 {
        self.input_tokens_uncached
            + self.input_tokens_cache_write
            + self.input_tokens_cache_read
    }

    pub fn cache_hit_rate(&self) -> f64 {
        let denom = self.total_input_tokens();
        if denom == 0 {
            return 0.0;
        }
        self.input_tokens_cache_read as f64 / denom as f64
    }
}
```

Mark any existing `input_tokens: u32` field deprecated if it exists. Remove it once downstream call sites are migrated (same PR).

### 5b. Anthropic provider parsing

In `moa-providers/src/anthropic.rs`, locate the response parser. Anthropic's `message_start` event carries:

```json
"usage": {
  "input_tokens": 25,
  "cache_creation_input_tokens": 1800,
  "cache_read_input_tokens": 12400,
  "output_tokens": 1
}
```

And the final `message_delta` carries updated `usage.output_tokens`. Populate `TokenUsage` as:

- `input_tokens_uncached = usage.input_tokens`
- `input_tokens_cache_write = usage.cache_creation_input_tokens` (may be absent → 0)
- `input_tokens_cache_read = usage.cache_read_input_tokens` (may be absent → 0)
- `output_tokens = usage.output_tokens`

Note: Anthropic's `input_tokens` field counts only uncached input (tokens not served from cache and not newly cached). Do not double-count.

### 5c. OpenAI provider parsing

OpenAI returns:

```json
"usage": {
  "prompt_tokens": 2048,
  "completion_tokens": 512,
  "prompt_tokens_details": { "cached_tokens": 1536 }
}
```

Populate:
- `input_tokens_cache_read = prompt_tokens_details.cached_tokens` (if present)
- `input_tokens_uncached = prompt_tokens - cached_tokens`
- `input_tokens_cache_write = 0` (OpenAI caches automatically, no user-controlled write; don't try to model it)
- `output_tokens = completion_tokens`

### 5d. Gemini provider parsing

Gemini returns:

```json
"usageMetadata": {
  "promptTokenCount": 2048,
  "candidatesTokenCount": 512,
  "cachedContentTokenCount": 1536
}
```

Populate the analogous fields. Both OpenAI and Gemini have `cache_write = 0` since cache population is implicit.

### 5e. Event schema migration

Extend `Event::BrainResponse`:

```rust
BrainResponse {
    text: String,
    model: String,
    #[serde(default)]
    input_tokens_uncached: u32,
    #[serde(default)]
    input_tokens_cache_write: u32,
    #[serde(default)]
    input_tokens_cache_read: u32,
    output_tokens: u32,
    cost_cents: u32,
    duration_ms: u64,
}
```

Old serialized events land with `cache_write = 0`, `cache_read = 0`, `uncached = <what used to be input_tokens>`. That's the right invariant — we observed zero cache activity before we instrumented.

Update the one or two call sites that construct `BrainResponse` to populate the new fields from `TokenUsage`.

### 5f. OTel span attributes

In the span that wraps `LLMProvider::complete` (step 39 added this), after the response is ready:

```rust
span.record("gen_ai.usage.input_tokens", usage.total_input_tokens());
span.record("gen_ai.usage.output_tokens", usage.output_tokens);
span.record("gen_ai.usage.cache_read_tokens", usage.input_tokens_cache_read);
span.record("gen_ai.usage.cache_write_tokens", usage.input_tokens_cache_write);
```

Also emit a single structured log line per completion:

```rust
tracing::info!(
    model = %response.model,
    input_uncached = usage.input_tokens_uncached,
    input_cache_read = usage.input_tokens_cache_read,
    input_cache_write = usage.input_tokens_cache_write,
    output = usage.output_tokens,
    cache_hit_rate = format!("{:.1}%", usage.cache_hit_rate() * 100.0),
    "completion received"
);
```

### 5g. Session-level aggregation

In `moa-orchestrator/src/local.rs`, after each turn, accumulate per-session totals. Store on `SessionMeta` (add a new table column in both SQLite and Postgres schemas — remember this lands before step 83's Postgres-only migration, so both dialects need the column):

```rust
SessionMeta {
    // existing fields...
    total_input_tokens_uncached: u32,
    total_input_tokens_cache_write: u32,
    total_input_tokens_cache_read: u32,
    // existing total_input_tokens becomes a computed accessor
}
```

Emit a `session.turn_completed` span attribute with the session's running `cache_hit_rate`.

### 5h. Tests

- Unit test: `TokenUsage::cache_hit_rate` with uncached=0, all three counters zero, and typical values.
- Unit test: each provider's response parser extracts all four fields correctly from a representative fixture.
- Event round-trip test: serialize a pre-79 `BrainResponse` (without new fields) and deserialize with the new schema — all three cache counters should be zero, `output_tokens` preserved.
- Integration test: add one more assertion to the step 78 test — after the multi-turn session, compute the cumulative cache hit rate and assert it's non-zero (proves step 76 is wired through end-to-end).

---

## 6. Deliverables

- [ ] `TokenUsage` has `input_tokens_{uncached,cache_write,cache_read}` and `output_tokens`; `total_input_tokens()` and `cache_hit_rate()` methods.
- [ ] Anthropic, OpenAI, and Gemini providers populate all four fields from real response data.
- [ ] `Event::BrainResponse` carries all four counters; old events deserialize cleanly with serde defaults.
- [ ] `SessionMeta` aggregates the four counters across a session. Both the SQLite and the Postgres schema add the new columns.
- [ ] OTel spans on LLM calls record `gen_ai.usage.*` attributes including cache variants.
- [ ] One structured log line per completion prints cache hit rate as a percentage.
- [ ] `moa-providers` unit tests cover parser correctness for all three providers with realistic fixtures.
- [ ] Step 78's integration test gains a cache-hit-rate non-zero assertion.

---

## 7. Acceptance criteria

1. A real call to `moa exec "hello"` (against Anthropic) produces a log line that includes `cache_hit_rate = X%`. The first turn typically shows 0% (cache is being populated); a second turn in the same session within 5 minutes shows > 50%.
2. `cargo test --workspace` passes including new unit tests.
3. OTel traces viewed in Jaeger/Tempo/Grafana show `gen_ai.usage.cache_read_tokens` on completion spans.
4. Querying the session store for a completed session returns non-zero `total_input_tokens_cache_read` for any Anthropic session longer than one turn.
5. Replaying a pre-step-79 session log still works (old events deserialize to `cache_read = cache_write = 0`).
6. If step 76's cache breakpoints are removed, the new metric correctly reads 0% hit rate — this is the regression signal we want.

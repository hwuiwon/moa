# 101 — Query Rewriting Pipeline Stage

## Purpose

Add a `QueryRewriter` context processor that rewrites multi-turn user queries into self-contained, unambiguous prompts before the brain's main LLM call. This resolves coreference ("fix that bug" → "fix the OAuth refresh token race condition in auth/refresh.rs"), decomposes compound queries, and extracts intent signals that downstream stages (MemoryRetriever, SkillInjector) can use for better retrieval.

The rewriter is a pipeline stage, not a separate Restate service. It runs as part of `prepare_turn_request` in `moa-brain`, using a fast small model (configurable, defaults to the cheapest available provider). It is **fail-open**: any failure falls back to the original query with zero user-visible impact.

## Prerequisites

- Pipeline stages 1–8 (identity through cache) all working.
- At least one LLM provider configured.
- `moa-brain/src/pipeline/mod.rs` supports async context processors.

## Read before starting

```
cat moa-brain/src/pipeline/mod.rs
cat moa-brain/src/pipeline/memory.rs
cat moa-brain/src/pipeline/skills.rs
cat moa-brain/src/pipeline/history.rs
cat moa-core/src/config.rs
cat moa-core/src/types.rs
```

## Architecture

### Where it fits in the pipeline

The rewriter runs **between SkillInjector (stage 4) and MemoryRetriever (stage 5)**. It rewrites the query *before* memory retrieval so that the MemoryRetriever searches with better keywords. It runs *after* skills so it can see which skills are available.

Pipeline order becomes:
1. IdentityProcessor
2. InstructionProcessor
3. ToolDefinitionProcessor
4. SkillInjector
5. **QueryRewriter** ← new
6. MemoryRetriever (uses rewritten query for search)
7. HistoryCompiler
8. RuntimeContextProcessor
9. CacheOptimizer

### Rewriter behavior

**Input**: Last N user messages from session history + current pending message.

**Output**: A `QueryRewriteResult` stored in `WorkingContext.metadata`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRewriteResult {
    /// The self-contained rewritten query. Never adds new entities.
    pub rewritten_query: String,
    /// Extracted intent classification.
    pub intent: QueryIntent,
    /// Optional sub-queries for compound tasks.
    pub sub_queries: Vec<String>,
    /// Tool names the rewriter thinks are relevant.
    pub suggested_tools: Vec<String>,
    /// Whether the rewriter thinks clarification is needed.
    pub needs_clarification: bool,
    /// If needs_clarification, the question to ask.
    pub clarification_question: Option<String>,
    /// Whether the rewriter actually ran or was skipped/failed.
    pub source: RewriteSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryIntent {
    Coding,
    Research,
    FileOperation,
    SystemAdmin,
    Creative,
    Question,
    Conversation,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewriteSource {
    Rewritten,
    Passthrough,  // skipped or failed, using original
}
```

### Skip conditions (no LLM call)

- Single-turn conversation AND query < 15 tokens
- Query starts with a tool-like verb ("read", "write", "search", "run", "deploy")
- Config `query_rewrite.enabled = false`

### Fail-open contract

- Timeout: 500ms hard cap on the rewriter LLM call
- Schema violation: if response doesn't parse as `QueryRewriteResult`, fall back
- Circuit breaker: if >5% of rewriter calls fail in 60s window, disable for 60s
- Any failure → `RewriteSource::Passthrough`, original query used, zero user impact

### Safety constraint

**The rewriter must never add new entities.** It can rephrase, decompose, or remove ambiguity, but it cannot fabricate context. The LLM prompt for the rewriter explicitly states: "Do not invent information not present in the conversation history."

## Steps

### 1. Add config

In `moa-core/src/config.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRewriteConfig {
    /// Whether query rewriting is enabled.
    pub enabled: bool,  // default: true
    /// Model to use for rewriting. Defaults to cheapest available.
    pub model: Option<String>,
    /// Hard timeout for the rewriter LLM call.
    pub timeout_ms: u64,  // default: 500
    /// Minimum token count in query to trigger rewriting.
    pub min_query_tokens: usize,  // default: 15
    /// Whether to skip rewriting on single-turn conversations.
    pub skip_single_turn: bool,  // default: true
    /// Circuit breaker: error rate threshold to disable rewriting.
    pub circuit_breaker_threshold: f64,  // default: 0.05
    /// Circuit breaker: window in seconds.
    pub circuit_breaker_window_secs: u64,  // default: 60
    /// Circuit breaker: cooldown in seconds after tripping.
    pub circuit_breaker_cooldown_secs: u64,  // default: 60
}
```

### 2. Add `QueryRewriteResult` types to `moa-core`

Add the types defined above. They belong in `moa-core` because `MemoryRetriever` and potentially the orchestrator need to read them from `WorkingContext.metadata`.

### 3. Implement `QueryRewriter` pipeline stage

Create `moa-brain/src/pipeline/query_rewrite.rs`:

```rust
pub struct QueryRewriter {
    config: QueryRewriteConfig,
    llm: Arc<dyn LLMProvider>,
    circuit_breaker: CircuitBreaker,
}

impl QueryRewriter {
    pub fn new(config: QueryRewriteConfig, llm: Arc<dyn LLMProvider>) -> Self { ... }

    fn should_skip(&self, ctx: &WorkingContext) -> bool {
        // Check: disabled, single-turn + short, tool-verb prefix, circuit open
    }

    async fn rewrite(&self, query: &str, history: &[ContextMessage]) -> Result<QueryRewriteResult> {
        // Build a minimal prompt with last 5 turns + current query
        // Call LLM with strict JSON schema, temperature 0
        // Parse response, validate no new entities added
        // Hard timeout via tokio::time::timeout
    }
}

#[async_trait]
impl ContextProcessor for QueryRewriter {
    fn name(&self) -> &str { "query_rewrite" }
    fn stage(&self) -> u8 { 5 }  // between skills (4) and memory (6)

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        if self.should_skip(ctx) {
            ctx.metadata.insert("query_rewrite".to_string(),
                serde_json::to_value(QueryRewriteResult::passthrough(
                    ctx.last_user_message().unwrap_or_default()
                ))?);
            return Ok(ProcessorOutput::default());
        }

        let original = ctx.last_user_message().unwrap_or_default();
        let history = ctx.recent_history(5);

        match tokio::time::timeout(
            Duration::from_millis(self.config.timeout_ms),
            self.rewrite(&original, &history)
        ).await {
            Ok(Ok(result)) => {
                self.circuit_breaker.record_success();
                ctx.metadata.insert("query_rewrite".to_string(),
                    serde_json::to_value(&result)?);
                Ok(ProcessorOutput {
                    tokens_added: 0,  // rewrite result goes to metadata, not messages
                    metadata: json!({ "rewrite_source": "rewritten", "intent": result.intent }),
                    ..Default::default()
                })
            }
            _ => {
                self.circuit_breaker.record_failure();
                tracing::warn!("query rewriter failed or timed out, falling back");
                ctx.metadata.insert("query_rewrite".to_string(),
                    serde_json::to_value(QueryRewriteResult::passthrough(&original))?);
                Ok(ProcessorOutput::default())
            }
        }
    }
}
```

### 4. Wire MemoryRetriever to use rewritten query

In `moa-brain/src/pipeline/memory.rs`, modify the search keyword extraction to check `ctx.metadata["query_rewrite"]` first:

```rust
fn extract_search_query(ctx: &WorkingContext) -> String {
    if let Some(rewrite) = ctx.metadata.get("query_rewrite") {
        if let Ok(result) = serde_json::from_value::<QueryRewriteResult>(rewrite.clone()) {
            if matches!(result.source, RewriteSource::Rewritten) {
                return result.rewritten_query;
            }
        }
    }
    // Fallback: use original last user message
    ctx.last_user_message().unwrap_or_default()
}
```

### 5. Implement circuit breaker

Simple sliding-window circuit breaker using atomics:

```rust
pub struct CircuitBreaker {
    failures: AtomicU32,
    successes: AtomicU32,
    last_reset: AtomicU64,  // epoch millis
    tripped_until: AtomicU64,
    threshold: f64,
    window_secs: u64,
    cooldown_secs: u64,
}

impl CircuitBreaker {
    pub fn is_open(&self) -> bool { ... }
    pub fn record_success(&self) { ... }
    pub fn record_failure(&self) { ... }
}
```

### 6. Build the rewriter prompt

The prompt sent to the small model:

```
You are a query rewriter for an AI agent system. Rewrite the user's query
into a self-contained, unambiguous request. Resolve pronouns and references
using the conversation history.

Rules:
- Do NOT invent information not present in the conversation history
- Do NOT add entities, file paths, or technical details not mentioned
- DO resolve "that", "it", "the bug", etc. to their concrete referents
- DO decompose compound requests into sub_queries
- Respond ONLY with valid JSON matching the schema below. No preamble.

Schema: {"rewritten_query": string, "intent": string, "sub_queries": [string],
"suggested_tools": [string], "needs_clarification": bool,
"clarification_question": string|null}

Conversation history (last 5 turns):
{history}

Current query:
{query}
```

### 7. Wire into pipeline construction

In `moa-brain/src/pipeline/mod.rs` and `moa-eval/src/setup.rs`, insert `QueryRewriter` at the correct position (after SkillInjector, before MemoryRetriever). Make it conditional on `config.query_rewrite.enabled`.

The rewriter needs its own `LLMProvider` instance — ideally the cheapest model available. Add a `resolve_rewriter_provider` function that picks from the configured providers, preferring Haiku-class models.

### 8. Tests

- Unit: skip on single-turn short query → passthrough
- Unit: multi-turn with "fix that" → rewritten to concrete reference
- Unit: compound query → decomposed into sub_queries
- Unit: timeout → passthrough with no error
- Unit: circuit breaker trips after threshold → subsequent calls skip
- Unit: circuit breaker resets after cooldown
- Unit: rewriter adds entity not in history → validation strips it (safety)
- Integration: full pipeline with rewriter → MemoryRetriever uses rewritten query

## Files to create or modify

- `moa-core/src/config.rs` — add `QueryRewriteConfig`
- `moa-core/src/types.rs` — add `QueryRewriteResult`, `QueryIntent`, `RewriteSource`
- `moa-brain/src/pipeline/query_rewrite.rs` — new file, the rewriter stage
- `moa-brain/src/pipeline/mod.rs` — register stage, add `pub mod query_rewrite`
- `moa-brain/src/pipeline/memory.rs` — use rewritten query for search
- `moa-eval/src/setup.rs` — wire rewriter into eval pipeline

## Acceptance criteria

- [ ] `cargo build` succeeds.
- [ ] Pipeline runs with rewriter enabled: multi-turn coreference resolved.
- [ ] Pipeline runs with rewriter disabled: identical behavior to before.
- [ ] Timeout at 500ms → graceful fallback, no user-visible error.
- [ ] Circuit breaker trips and recovers correctly.
- [ ] MemoryRetriever uses rewritten query for search when available.
- [ ] Eval tests pass with rewriter in the pipeline.
- [ ] Rewriter never adds entities not present in conversation history.

## Notes

- **Cost**: ~500 input + 200 output tokens at Haiku pricing ≈ $0.0004/call. Pays for itself if it prevents even 5% of failed tool calls.
- **Latency**: 100–400ms typical. The 500ms hard timeout prevents tail latency from impacting the main turn.
- **The rewriter does NOT modify messages in the context.** It stores results in `ctx.metadata` where downstream stages can optionally read them. The original user message is always preserved in the conversation history.
- Clarification support is a future extension — for now, `needs_clarification` is computed but not acted on. The brain can use it if it wants.
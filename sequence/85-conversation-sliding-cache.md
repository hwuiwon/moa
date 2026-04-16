# Step 85 — Conversation Sliding Window Cache Breakpoint

_Step 76 cached the system prompt and tool definitions. Step 84 made them byte-stable. Now cache the growing conversation history. Add a third cache breakpoint on the last tool_result in the conversation, with 5-minute TTL, and manage it as the conversation grows to stay within Anthropic's 20-block lookback window._

---

## 1. What this step is about

Anthropic prompt caching allows up to 4 `cache_control` breakpoints per request. Steps 76 and 84 use two or three of them:
- BP1 on the identity + guardrails system prompt (1-hour TTL)
- BP2 on workspace instructions + skills (1-hour TTL)
- BP3 on the tool definitions block (1-hour TTL)

For a session on turn 5 with 4 prior turns of history, the conversation history block is ~10K tokens of user messages, assistant responses, tool calls, and tool results. Without a cache breakpoint on it, we pay full uncached input price for all 10K every turn.

Adding BP4 on the last tool_result (or assistant message) caches the conversation prefix up to that point. Turn N+1 pays 0.1× for all N turns of history. With Anthropic's 5-minute ephemeral TTL auto-extending on every hit, an active session keeps the whole conversation cached for the duration of the work.

The tricky part: Anthropic's cache lookback is **20 content blocks max** from any `cache_control` mark. If the conversation grows beyond that, the breakpoint's lookback window doesn't cover old history, meaning cache hits stop forming. We need to either insert intermediate breakpoints or keep the conversation compact (step 88 handles compaction).

---

## 2. Files to read

- `moa-brain/src/pipeline/history.rs` — `compile_messages()`. Where the conversation history is built.
- `moa-brain/src/pipeline/cache_optimizer.rs` (step 76 stage 7) — where breakpoints are placed. We extend.
- `moa-providers/src/anthropic.rs` — how cache_control blocks are serialized into the Anthropic request.
- Anthropic docs (https://docs.claude.com/en/docs/build-with-claude/prompt-caching) — confirmed behaviors: 4 breakpoint max, 5-min / 1-hour TTLs, 20-block lookback.

---

## 3. Goal

1. After BP1/BP2/BP3 (step 76 + step 84), add BP4 on the most recent tool_result message in the conversation history.
2. BP4 uses 5-minute TTL (ephemeral).
3. When the conversation exceeds 18 content blocks past BP4's position, the cache optimizer re-plans breakpoints to keep the lookback window intact. Options: move BP4 forward AND set one of BP1–BP3 to a lower-TTL rolling position; or do full compaction (step 88).
4. Cache hit rate on turn 10+ of a single interactive session approaches 85%.

---

## 4. Rules

- **BP4 sits on the message that will NOT be appended-to.** Putting `cache_control` on the last message in the request is pointless — nothing gets cached from it for future turns. The correct position is the LAST tool_result BEFORE the current user turn. That message's byte prefix is frozen; future turns will append after it, and those appends become the "new" tail while everything up to BP4 is cached.
- **5-minute TTL, not 1-hour.** The conversation tail changes often. 1-hour cache write premium (2× cost) is wasted; 5-min (1.25× write premium) is the right choice here.
- **20-block lookback is a hard limit.** Track block count from BP4. If next_turn_blocks_added_since_bp4 > 18, re-plan.
- **Never exceed 4 cache_control blocks total.** Anthropic rejects requests with more. If adding BP4 would overflow, the cache_optimizer must drop one of the earlier breakpoints — preferably BP2 (workspace instructions + skills), because those live in shorter-cycle cache behavior anyway, or accept a cache miss on that region.
- **Breakpoints only make sense on boundaries between "frozen" and "growing" content.** Putting a breakpoint at the very start of `[user_turn N+1]` has no effect — cache matches on everything BEFORE the breakpoint, and we already have BP3 handling that.

---

## 5. Tasks

### 5a. Extend `CacheOptimizer` (stage 7)

The stage-7 optimizer already has a list of `cache_breakpoints: Vec<usize>` representing indexes into the message array. Extend it:

```rust
pub struct CacheOptimizer {
    max_breakpoints: usize, // 4
    min_blocks_before_bp4: usize, // 3 — don't bother on very short sessions
    max_blocks_after_bp4: usize, // 18 — stay inside the 20-block lookback with margin
}

impl ContextProcessor for CacheOptimizer {
    fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        self.place_bp1_identity(ctx);          // already done in step 76
        self.place_bp2_workspace_skills(ctx);  // already done in step 76
        self.place_bp3_tool_defs(ctx);         // already done in step 76
        self.place_bp4_conversation(ctx);      // NEW

        if self.blocks_after_bp4(ctx) > self.max_blocks_after_bp4 {
            self.replan_on_lookback_overflow(ctx);
        }

        Ok(ProcessorOutput { /* ... */ })
    }
}
```

### 5b. Placing BP4

Walk the message array from the end backward. Find the last message that is a tool_result OR the last assistant message, whichever is later. Mark `cache_control: { type: "ephemeral", ttl: "5m" }` on it.

Skip the current user turn's messages (they are the "tail" — caching them is meaningless).

If there's no tool_result or assistant message yet (turn 1 of a new session), don't place BP4. It's wasted on fewer than `min_blocks_before_bp4` blocks.

### 5c. Lookback overflow re-planning

Simplest correct strategy: when `blocks_since_bp4 > 18`, move BP4 forward to the most recent tool_result, accepting that the oldest chunk of conversation falls out of cache.

More sophisticated: insert BP4 at a middle position and also insert a second "rolling" breakpoint at the new recent tool_result — that preserves mid-conversation cache. But this conflicts with "max 4 breakpoints". To make room, drop BP2 (workspace instructions), accepting ~500 tokens of re-reading. Usually a fine trade.

Implement the simplest first (move BP4 forward). Add a TODO for the two-breakpoint strategy if metrics show it's needed.

### 5d. Provider-side serialization

In `moa-providers/src/anthropic.rs`, the `CompletionRequest → Anthropic JSON` serializer must honor breakpoints on message content. Anthropic's schema places `cache_control` inside a content block:

```json
{
  "role": "user",
  "content": [
    {"type": "tool_result", "tool_use_id": "...", "content": "..."},
    {"type": "text", "text": "", "cache_control": {"type": "ephemeral", "ttl": "5m"}}
  ]
}
```

Actually the conventional placement is on the LAST content block of the message. For a tool_result message, it's on the tool_result block itself. Verify the current serializer does this correctly; extend if needed.

If the provider serializer currently only supports breakpoints on the system prompt or on tool definitions (step 76 behavior), extend it to support breakpoints on message-level content blocks too.

### 5e. Tests

- Unit: given a pipeline with 4 prior turns of history, place_bp4 marks the last tool_result message.
- Unit: given a fresh session (turn 1), place_bp4 is a no-op.
- Unit: given 25 blocks of content past BP4, `replan_on_lookback_overflow` moves BP4 to a position where `blocks_after_bp4 ≤ 18`.
- Integration (extend step 78): after 4 turns, inspect the recorded Anthropic request bodies. Assert the request for turn 5 has exactly 4 `cache_control` blocks, with TTLs matching: 1h, 1h, 1h, 5m.
- Metric assertion (optional, requires network): across a real 6-turn session, the step-79 cache hit rate by turn 6 is ≥ 80%.

### 5f. Documentation

Update `moa/docs/prompt-caching-architecture.md` (from step 84) with the 4-breakpoint layout diagram:

```
[BP1: identity + guardrails]     TTL=1h
[BP2: workspace + skills]        TTL=1h
[BP3: tool definitions]          TTL=1h
[BP4: last tool_result]          TTL=5m  <-- rolls forward as conversation grows
[runtime context: system-reminder]        <-- NOT cached
[current user turn messages]              <-- NOT cached
```

---

## 6. Deliverables

- [ ] `CacheOptimizer::place_bp4_conversation` implementation.
- [ ] Lookback-overflow re-planning logic (simple forward-move strategy).
- [ ] Anthropic provider serializer handles message-level `cache_control` blocks.
- [ ] Unit tests for BP4 placement edge cases.
- [ ] Step 78 integration test asserts the 4-breakpoint structure on turn 5+.
- [ ] Caching architecture doc updated with the 4-BP diagram.

---

## 7. Acceptance criteria

1. A turn-5 real session produces an Anthropic request with 4 `cache_control` blocks (3 × 1h TTL, 1 × 5m TTL).
2. Step 79's cache hit rate metric on turn 5+ reaches ≥ 80% for interactive sessions that stay under 20 blocks.
3. For sessions that grow past 20 blocks, metric shows sustained ≥ 60% hit rate (because BP4 keeps rolling forward, the oldest chunk falls out, but most of the conversation remains cached).
4. `cargo test --workspace` green.
5. No Anthropic API rejections due to "too many cache_control blocks" (we never exceed 4).
6. BP4 is NOT placed when the conversation has fewer than 3 prior blocks (turn 1-2 of a new session).

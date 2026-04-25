## Prompt Caching Architecture

MOA treats the prompt prefix as a cache key.

Anthropic prompt caching matches on exact-prefix byte equality. If any byte in
the cached prefix changes between turns, the cache misses and the request pays
full input cost again. Because of that, the early pipeline stages must stay
static for identical session inputs.

### Stable Prefix

The long-lived static prefix is produced by the byte-stable pipeline stages:

1. `IdentityProcessor`
2. `InstructionProcessor`
3. `ToolDefinitionProcessor`
4. `SkillInjector`

These stages must not render per-turn dynamic values such as:

- current datetime
- current working directory
- current git branch
- current user identity
- counters that change every turn
- workspace-specific ranking stats that reorder tools or skills

If a stage above needs to reference dynamic runtime state, it should use a
placeholder in static text and rely on the runtime reminder described below.

### Rolling Conversation Prefix

Conversation history is cached separately from the static prefix.

MOA uses four logical cache regions:

1. `BP1` — identity + guardrails (`1h`)
2. `BP2` — workspace instructions + skills (`1h`)
3. `BP3` — tool definitions (`1h`)
4. `BP4` — the last frozen assistant/tool message in conversation history (`5m`)

`BP4` rolls forward as the conversation grows so that the cached conversation
tail stays within Anthropic's 20-block lookback window. The deepest breakpoint
is therefore not the static prefix; it is the rolling conversation prefix.

### Dynamic Tail

All per-turn runtime state belongs in the dynamic tail:

- `QueryRewriter` stores rewritten-query and task-transition metadata without
  altering the stable prefix.
- `MemoryRetriever` injects relevant memory after query rewriting and before
  history compilation.
- `HistoryCompiler` emits replayed conversation, checkpoints, recent turns,
  and segment events.
- `RuntimeContextProcessor` emits the runtime reminder immediately before the
  current user turn.

`RuntimeContextProcessor` emits a single trailing user-role message in the form:

```text
<system-reminder>
Current date: 2026-04-16
Current workspace: moa
Current working directory: /Users/example/Github/moa
Current git branch: main
Current user: alice
</system-reminder>
```

This reminder is inserted after the cacheable static and conversation prefix
boundaries and before the current user turn. That keeps the early prompt
byte-stable while still giving the model the runtime facts it needs for the
active turn.

### Provider Mapping

- Anthropic uses explicit `cache_control` markers with `1h` and `5m` TTLs.
- OpenAI does not use message-level breakpoints, but it benefits from the same
  prompt layout because prompt caching matches exact prefixes. MOA should keep
  the static prefix stable and the dynamic tail at the end, and provide a
  stable `prompt_cache_key` for repeated prefixes.
- Gemini benefits from the same prompt layout for implicit caching. Explicit
  Gemini cached-content resources are a separate optimization and should not be
  mixed into the byte-stable prompt stages.

### Rules For Future Changes

When adding prompt content:

- Put static instructions in the early pipeline stages.
- Keep query rewriting, retrieved memory, replayed history, and runtime context
  out of the stable prefix.
- Preserve the current dynamic order: query rewrite, memory, history, runtime
  context, compactor, cache optimizer.
- Put dynamic session or turn state in `RuntimeContextProcessor`.
- Keep tool definitions sorted deterministically by tool name.
- Keep rendered skill metadata sorted deterministically by skill name.
- Do not include usage counters, timestamps, or success-rate fields in the
  cached prefix.

### Verification

Use the stable-prefix test before merging prompt changes:

```bash
cargo test -p moa-brain --test stable_prefix
```

That test compiles the same pipeline twice and asserts the cached prefix bytes
match exactly.

For a live cache validation against Anthropic, run:

```bash
cargo test -p moa-brain --test live_cache_audit -- --ignored --nocapture
```

Expected behavior:

- the stable prefix fingerprint is reused across turns
- turn 1 is typically cold
- later turns in the same session should report non-zero cached input tokens

If the stable-prefix test fails or live cache reads stay at zero after the
prefix is warm, first inspect the static stages for newly introduced dynamic
content.

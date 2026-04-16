## Prompt Caching Architecture

MOA treats the prompt prefix as a cache key.

Anthropic prompt caching matches on exact-prefix byte equality. If any byte in
the cached prefix changes between turns, the cache misses and the request pays
full input cost again. Because of that, the early pipeline stages must stay
static for identical session inputs.

### Stable Prefix

The cached prompt prefix is produced by the static pipeline stages:

1. `IdentityProcessor`
2. `InstructionProcessor`
3. `ToolDefinitionProcessor`
4. `SkillInjector`
5. `MemoryRetriever`
6. `HistoryCompiler`

These stages must not render per-turn dynamic values such as:

- current datetime
- current working directory
- current git branch
- current user identity
- counters that change every turn
- workspace-specific ranking stats that reorder tools or skills

If a stage above needs to reference dynamic runtime state, it should use a
placeholder in static text and rely on the runtime reminder described below.

### Dynamic Tail

All per-turn runtime state belongs in `RuntimeContextProcessor`.

That processor emits a single trailing user-role message in the form:

```text
<system-reminder>
Current date: 2026-04-16
Current workspace: moa
Current working directory: /Users/example/Github/moa
Current git branch: main
Current user: alice
</system-reminder>
```

This reminder is inserted after the cached prefix boundary and before the
current user turn. That keeps the system prompt byte-stable while still giving
the model the runtime facts it needs for the active turn.

### Rules For Future Changes

When adding prompt content:

- Put static instructions in the early pipeline stages.
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

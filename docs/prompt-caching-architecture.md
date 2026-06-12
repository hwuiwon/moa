## Prompt Caching Architecture

MOA treats prompt ordering as the cache contract.

Prompt caches are owned by provider APIs and by the LLM gateway/provider layer.
The brain does not emit provider-specific cache breakpoints, TTLs, retention
policies, or Gemini cached-content names. Its job is to keep repeated prompt
sections at the beginning and per-turn sections near the active turn.

### Stable Prefix

The long-lived static prefix is produced by the byte-stable pipeline stages:

1. `IdentityProcessor`
2. `InstructionProcessor`
3. `ToolDefinitionProcessor`

These stages must not render per-turn dynamic values such as:

- current datetime
- current working directory
- current git branch
- current user identity
- counters that change every turn
- query-shaped ranking stats that reorder tools or skills
- retrieved memory
- current turn text

If a stage above needs to reference dynamic runtime state, it should use a
placeholder in static text and rely on the runtime reminder described below.

### Dynamic Tail

All per-turn runtime state belongs in the dynamic tail:

- `QueryRewriter` stores rewritten-query and task-transition metadata without
  altering the stable prefix.
- `SkillInjector` injects a compact selected-skill manifest after query
  rewriting. The selection can depend on query keywords, tenant-level learning,
  use count, and recency, so it is not part of the stable prefix.
- `DigestProcessor` injects standing user/workspace memory after query
  rewriting and skill selection.
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

This reminder is inserted after the stable prefix and before the current user
turn. That keeps the early prompt byte-stable while still giving the model the
runtime facts it needs for the active turn.

### Provider Mapping

- OpenAI prompt caching is automatic for supported models. The OpenAI adapter
  may derive a stable `prompt_cache_key` from the ordered static prefix, but
  the shared request type does not expose OpenAI cache policy.
- Anthropic prompt caching is provider-owned. The Anthropic adapter may enable
  top-level automatic `cache_control` for cache-eligible requests and may add
  one provider-owned marker at the stable prefix boundary. The brain does not
  emit block-level markers or TTL policy.
- Gemini uses implicit caching for the default request path. Explicit
  `cachedContents` resources are a separate provider feature and are not part
  of the default MOA prompt compilation path.

### Rules For Future Changes

When adding prompt content:

- Put static instructions in the early pipeline stages.
- Keep query rewriting, retrieved memory, replayed history, and runtime context
  out of the stable prefix.
- Preserve the current dynamic order: query rewrite, skills, standing memory
  digest, graph memory, history, runtime context, compactor.
- Put dynamic session or turn state in `RuntimeContextProcessor`.
- Keep tool definitions sorted deterministically by tool name.
- Keep rendered skill metadata deterministic, but do not place selected skills
  in the stable prefix.
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
prefix is warm, first inspect the static stages and provider request mapping
for newly introduced dynamic content before changing retrieval or history logic.

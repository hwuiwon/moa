# Step 84 — Prompt Caching Architecture: Static Prefix, Dynamic Tail

_The single biggest cache hit rate win: make the system prompt byte-identical across turns by relocating all dynamic content (datetime, workspace name, user name, runtime context) into a trailing user message. ProjectDiscovery moved from 7% to 74% cache hit rate with this one change._

---

## 1. What this step is about

Anthropic's prompt cache matches on exact-prefix byte equality. If ANY byte in the system prompt differs between turn N and turn N+1, the cache misses entirely and we pay 1× for every input token.

Today, MOA's `IdentityProcessor` + `InstructionProcessor` produce a system prompt that almost certainly includes dynamic values: current datetime, workspace name, workspace path, sometimes a user identifier. Each of those invalidates the cache every turn. We can have step 76 perfectly wired and still see ~10% hit rates because the cache never forms.

The fix is a well-known pattern:

1. System prompt = **only** content that is byte-identical across all turns of all sessions for this model configuration.
2. All per-session, per-turn dynamic content moves to a `<system-reminder>` block appended as the LAST user message before the current user turn.

ProjectDiscovery documented this trick as the single change that took their cache hit rate from 7% to 74%.

---

## 2. Files to read

- `moa-brain/src/pipeline/identity.rs` — where the identity prompt + coding guardrails live. Step 70 added guardrails here. Any dynamic content that slipped in during step 70 must come out.
- `moa-brain/src/pipeline/instruction_processor.rs` (or wherever stage 2 lives) — workspace instructions, user preferences.
- `moa-brain/src/pipeline/tool_definition_processor.rs` — tool schemas. Must be sorted stably; tool choice must not depend on turn-specific context (it already shouldn't, but confirm).
- `moa-brain/src/pipeline/skill_injector.rs` (stage 4) — active skill metadata. Per-workspace-static is OK; per-turn-dynamic must move.
- `moa-brain/src/pipeline/memory_retriever.rs` (stage 5) — this is ALREADY where dynamic content lives today. We extend it.
- Any call site that interpolates `Utc::now()` into prompt text. Grep for it.

---

## 3. Goal

1. The system prompt (stages 1–4 output) is byte-identical across every turn of every session whose (model, workspace instructions, skills loadout) tuple is identical.
2. All dynamic content — current datetime (per-turn), workspace name, workspace absolute path, user name, git branch, runtime environment flags — moves to a single `RuntimeContextProcessor` that emits one trailing `<system-reminder>` user message positioned AFTER the cached conversation breakpoint (from step 85, next step) and BEFORE the current user turn.
3. A startup assertion (or test) detects drift: compile the pipeline twice with identical inputs at T=0 and T+5s, assert the byte-identical-prefix property.
4. Step 79's cache hit rate metric shows a measurable jump after this step lands. Expect +30 to +60 percentage points.

---

## 4. Rules

- **The system prompt is a cache key.** Treat it as if adding a byte breaks billing. Anything turn-dependent cannot live there.
- **Placeholders in the system prompt, real values in the tail.** If the identity prompt needs to reference "the current date," write it as `[current date: see Runtime Context]`. Do not interpolate the actual date.
- **Datetime freezing.** The `RuntimeContextProcessor` emits a datetime value that's stable for at least the duration of one turn. Don't call `Utc::now()` multiple times within a single turn compile — it will drift between log lines and break debugging. Capture once per turn.
- **Workspace instructions are static per-workspace, so they CAN live in the system prompt.** They move when the workspace changes, not per-turn. Verify by diff-hashing `workspace_instructions` across two consecutive turns in the same session; they should match.
- **Skills metadata must be sorted deterministically.** If skill order in the stage-4 output depends on `last_used` or `use_count`, those values change every turn and break caching. Sort by skill name (lexicographic). This was an issue flagged for future work; address it here.
- **Tool definitions must be sorted deterministically.** Re-verify from step 76: `sort_json_keys` is applied, tools are ordered by name. No exceptions.

---

## 5. Tasks

### 5a. Audit the current system prompt for dynamic content

Write a small test in `moa-brain/tests/stable_prefix.rs`:

```rust
#[tokio::test]
async fn system_prompt_bytes_are_stable_across_compiles() {
    let ctx1 = compile_pipeline(fixed_session_snapshot()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ctx2 = compile_pipeline(fixed_session_snapshot()).await;

    let prefix1 = system_prompt_bytes(&ctx1);
    let prefix2 = system_prompt_bytes(&ctx2);

    assert_eq!(prefix1, prefix2, "system prompt must be byte-identical across compiles");
}
```

Run it before making changes. It will likely fail. That's the baseline.

Now enumerate everything that differs by diffing the two prompts. Typical culprits:
- `Utc::now()` interpolation
- Workspace path with absolute paths (ok if workspace is static)
- Tool descriptions that include dynamic counters
- Skill metadata with `last_used` / `use_count`

### 5b. Rewrite identity + instruction processors to be static

Move any dynamic insertion out. Replace with placeholder text:

```
Today's date: [see Runtime Context]
Workspace: [see Runtime Context]
Current user: [see Runtime Context]
```

Keep everything else — identity, coding guardrails from step 70, tool philosophy language — completely static.

### 5c. Create `RuntimeContextProcessor` as stage 5.5 or 6.5

Add a new pipeline stage that runs AFTER `MemoryRetriever` and BEFORE `HistoryCompiler`, OR as a final stage that emits one last trailing message before the current user turn (depending on where the current user turn is placed in the pipeline).

Actually, the cleanest shape: the RuntimeContext emission happens as the FIRST user message in the turn (or as the last system message after the cached stage-5 memory block), immediately preceded by cache_control breakpoint. Structure:

```
[system] identity + guardrails                        ← BP1 (cached, 1-hour TTL)
[system] workspace instructions + skills              ← BP2 (cached, 1-hour TTL)
[tool definitions block]                              ← BP3 (cached, 1-hour TTL, from step 76)
[cached conversation messages from prior turns]       ← BP4 (cached, 5-min TTL, grown via step 85)
[user-role system-reminder: runtime context]          ← NOT CACHED (changes every turn)
[current user turn's messages]
```

The `<system-reminder>` format, following Anthropic's and Claude Code's convention:

```
<system-reminder>
Current date: 2026-04-16
Current working directory: /Users/hwuiwon/github/moa
Current git branch: main
</system-reminder>
```

Wrap that text in a single user-role message (Anthropic's schema requires tool results and runtime context to be either user or tool messages; user role is simplest).

### 5d. `RuntimeContextProcessor` implementation

```rust
// moa-brain/src/pipeline/runtime_context.rs
pub struct RuntimeContextProcessor {
    clock: Arc<dyn Clock>,
}

impl ContextProcessor for RuntimeContextProcessor {
    fn name(&self) -> &str { "runtime_context" }
    fn stage(&self) -> u8 { 6 } // or whatever keeps it after memory, before history

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        let now = self.clock.now(); // captured once per turn
        let workspace = ctx.workspace.name.clone();
        let workspace_path = ctx.workspace.root_path.display().to_string();
        let user = ctx.user.display_name.clone().unwrap_or_default();
        let git_branch = detect_git_branch(&ctx.workspace.root_path).await;

        let reminder = format!(
            "<system-reminder>\n\
             Current date: {date}\n\
             Current working directory: {cwd}\n\
             {branch}\
             {user}\
             </system-reminder>",
            date = now.format("%Y-%m-%d").to_string(),
            cwd = workspace_path,
            branch = git_branch.map(|b| format!("Current git branch: {b}\n")).unwrap_or_default(),
            user = if user.is_empty() { String::new() } else { format!("Current user: {user}\n") },
        );

        ctx.push_user_message(reminder);
        Ok(ProcessorOutput {
            tokens_added: estimate_tokens(&reminder),
            items_included: vec!["runtime_context".into()],
            ..Default::default()
        })
    }
}
```

The `Clock` trait is a simple injectable:

```rust
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

pub struct SystemClock;
impl Clock for SystemClock { fn now(&self) -> DateTime<Utc> { Utc::now() } }
```

Tests get a `FixedClock`. Production gets `SystemClock`.

### 5e. Remove `Utc::now()` calls from stages 1–4

`grep -n 'Utc::now\|chrono::now\|SystemTime' moa-brain/src/pipeline/*` should return zero hits in stages 1–4 after this step. Every datetime surfaces through `RuntimeContextProcessor` only.

### 5f. Skills stage: deterministic ordering

In `SkillInjector`, replace any sort by `last_used` or `use_count` with:

```rust
skills.sort_by(|a, b| a.name.cmp(&b.name));
```

Do not include `use_count` or `last_used` in the rendered skill metadata text. Those change between turns. If they're useful for `memory_search`, expose them there, not in the prompt.

### 5g. Byte-stability assertion in startup (dev builds)

In debug builds only, call the byte-stability check during orchestrator startup:

```rust
#[cfg(debug_assertions)]
fn assert_prefix_stability() {
    let p1 = compile_pipeline(snapshot_a()).system_prompt_bytes();
    let p2 = compile_pipeline(snapshot_a()).system_prompt_bytes();
    if p1 != p2 {
        tracing::error!("prompt prefix is not byte-stable — cache will miss on every turn!");
        // don't panic in dev, just loud-log
    }
}
```

Not in release builds (startup cost).

### 5h. Tests

- `system_prompt_bytes_are_stable_across_compiles` — the one from 5a, now expected to pass.
- `runtime_context_changes_between_turns` — assert the tail message differs between turns when time advances or workspace changes.
- `skill_order_is_deterministic` — hash the stage-4 output across two compiles; expect match.
- Extend step 78's integration test: after 3 turns, inspect the recorded Anthropic requests. For turn 2 and turn 3, the first ~N bytes of each request body must byte-match. (N is "end of the tool definitions block"; see step 85 for extending the match to conversation history.)

### 5i. Documentation

Write `moa/docs/prompt-caching-architecture.md` explaining:
- Why the system prompt must be byte-stable
- What lives in `RuntimeContextProcessor`
- How to add new dynamic content (add it to `RuntimeContextProcessor`, NOT to any earlier stage)
- How to verify a change preserves the cache (run the stable-prefix test)

This is the doc people will reference when adding future features. Put a warning at the top of `identity.rs`:

```rust
// WARNING: This file produces the cached system prompt.
// DO NOT add dynamic content here (datetime, workspace path, etc.).
// Dynamic content goes in RuntimeContextProcessor.
// See moa/docs/prompt-caching-architecture.md
```

---

## 6. Deliverables

- [ ] Stages 1–4 of the pipeline produce byte-stable output for identical inputs.
- [ ] `RuntimeContextProcessor` added at stage 5.5 or 6.5 emitting a single `<system-reminder>` user message.
- [ ] `Clock` abstraction + `SystemClock` + `FixedClock` for tests.
- [ ] Skills stage sorted by name; no turn-dependent metadata in the rendered output.
- [ ] `stable_prefix.rs` test passes.
- [ ] Extended step 78 integration test asserts byte-identical prefix across turns.
- [ ] `moa/docs/prompt-caching-architecture.md` written.
- [ ] Warning comments at the top of `identity.rs` and other frozen stages.

---

## 7. Acceptance criteria

1. `system_prompt_bytes_are_stable_across_compiles` passes.
2. Running a 3-turn real session against Anthropic, the cache hit rate from step 79's metric is ≥ 50% on turn 2 and ≥ 70% on turn 3. Measured before: likely <20% even with step 76.
3. Turn 2's `CompletionRequest` body matches turn 1's body byte-for-byte from the start of the system prompt through the end of the tool definitions block. (Assertion in the step 78 integration test.)
4. Turn 2's `<system-reminder>` text contains the current date and workspace path; turn 1's text is different from turn 2's when `FixedClock` advances between them.
5. `cargo test --workspace` green.
6. `grep 'Utc::now' moa-brain/src/pipeline/{identity,instruction_processor,tool_definition,skill_injector}*.rs` returns no matches.
7. Step 79's session-level cache hit rate dashboard shows a step-function increase on the day this lands.

# Step 88 — Tiered Context Compaction

_Three-tier compaction that keeps long sessions inside the cache lookback window and inside the model's context limit. Tier 1 is zero-cost deterministic cleanup. Tier 2 is cache-aware trimming. Tier 3 is full LLM summarization. Fires at 40% / 60% / 90% context-capacity thresholds._

---

## 1. What this step is about

Steps 84–87 built the caching + snapshot infrastructure. That infrastructure works well until the conversation grows past two soft limits:

1. **Anthropic's 20-block cache lookback** (step 85). Once conversation has > 20 content blocks past BP4, the cache window can no longer cover the oldest chunk.
2. **The model's effective context limit.** 200K tokens on Claude Sonnet 4.6 sounds huge but gets eaten fast: a single 19K-line Python file is ~76K tokens, and agents re-read files.

Claude Code's engineering has converged on a 3-tier compaction model. MOA step 77's file-read dedup is already a piece of Tier 1. This step builds the rest.

**Tier 1 — Deterministic cleanup (no LLM call, ~free):**
- Keep only the most recent N file_read results verbatim (step 77 already does this for path-dedup; extend to "N most recent full-file reads").
- Collapse successive bash tool calls that produced identical output.
- Replace tool_result content older than M turns with `[tool result elided by compaction]` while keeping the structural tool_use_id intact.

**Tier 2 — Cache-aware trim (no LLM call):**
- When the conversation approaches 18 blocks past BP4, trim oldest messages up to the nearest compaction-safe boundary, re-anchor BP4 at the new head of conversation.
- Only applies to messages whose tool_use_id is NOT referenced in any later message. Safe-to-drop tool results.

**Tier 3 — LLM summarization (calls the LLM with a summarization prompt):**
- When context usage crosses 90% of the model's window, or at an explicit `/compact` signal.
- Summarize turns [0..N-5] into a `[conversation summary]` block; keep the 5 most recent turns verbatim.
- Summary prompt is itself cache-friendly (static template + dynamic conversation).
- Result gets persisted as a `Event::Checkpoint { summary, covers_events_up_to }` — rebuilds survive replay.

---

## 2. Files to read

- `moa-brain/src/compaction.rs` — if it exists from earlier steps; otherwise new.
- `moa-brain/src/pipeline/history.rs` — stage 6 integrates with compaction output.
- `moa-core/src/types/event.rs` — `Event::Checkpoint` already exists; verify the schema.
- Step 87 `FileReadDedupState` — compaction respects the dedup set.
- Anthropic context-engineering guide (https://platform.claude.com/cookbook/tool-use-context-engineering-context-engineering-tools).

---

## 3. Goal

1. Every turn, before the LLM call, a compaction pass runs and emits a `CompactionReport` with tier(s) applied, tokens reclaimed, messages affected.
2. Tier 1 runs on every turn (cheap, idempotent). Tier 2 runs when `blocks_past_bp4 > 14`. Tier 3 runs when `total_input_tokens > 0.9 * model.context_limit`, or on explicit request.
3. After Tier 3, the session event log contains an `Event::Checkpoint` that records the summary and what it replaced. Future replays (with snapshot disabled or invalidated) deterministically reproduce the same compacted context.
4. `cache_hit_rate` (step 79) remains above 60% even for sessions that triggered Tier 3 — summary itself is cached for remaining turns.
5. Total input tokens per turn never exceeds a configured ceiling (default: 0.8 × model.context_limit).

---

## 4. Rules

- **Tier 1 is deterministic and idempotent.** Running it twice produces the same output. No randomness, no LLM.
- **Tier 2 may drop content.** It must never drop a tool_result whose tool_use_id is referenced in a later tool_call or assistant message (that would break the provider's API contract).
- **Tier 3 is the only tier that writes an event.** Tiers 1 and 2 operate only on the compiled `Vec<ContextMessage>`; they do not mutate the event log. The event log stays the source of truth.
- **Summarization prompt is a cached asset.** The system prompt for the summarizer call is a static template + dynamic conversation. Cache the template.
- **Compaction preserves the 5 most recent turns verbatim.** Configurable via `compaction.recent_turns_verbatim`, default 5.
- **Compaction is idempotent on restart.** If step 86's snapshot is lost and a session is replayed from events, Tier 3 checkpoints are applied deterministically (the summary was persisted).
- **Compaction does NOT fire inside the snapshot boundary.** If step 86 loaded a snapshot, compaction decisions already made (reflected in the snapshot) stand. Compaction only considers the delta.

---

## 5. Tasks

### 5a. Define `CompactionTier` and `CompactionReport`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionTier {
    Tier1Deterministic,
    Tier2CacheAware,
    Tier3Summarization,
}

#[derive(Debug, Clone, Default)]
pub struct CompactionReport {
    pub tiers_applied: Vec<CompactionTier>,
    pub tokens_before: usize,
    pub tokens_after: usize,
    pub messages_elided: usize,
    pub summary_text: Option<String>, // populated only if Tier 3 ran
    pub covers_events_up_to: Option<SequenceNum>,
}

impl CompactionReport {
    pub fn tokens_reclaimed(&self) -> usize {
        self.tokens_before.saturating_sub(self.tokens_after)
    }
}
```

### 5b. `Compactor` as a pipeline stage (between history and cache_optimizer)

```rust
// moa-brain/src/pipeline/compaction.rs

pub struct Compactor {
    config: CompactionConfig,
    llm: Arc<dyn LLMProvider>, // used for Tier 3 only
}

pub struct CompactionConfig {
    pub tier2_trigger_blocks_past_bp4: usize, // default 14 (leaves 4 blocks of headroom)
    pub tier3_trigger_fraction: f64,          // default 0.9
    pub recent_turns_verbatim: usize,         // default 5
    pub model_context_limit: usize,           // from model capabilities
    pub max_input_tokens_per_turn: usize,     // default: 0.8 × context_limit
}

#[async_trait]
impl ContextProcessor for Compactor {
    fn name(&self) -> &str { "compactor" }
    fn stage(&self) -> u8 { 7 } // or wherever keeps it after history, before cache_optimizer

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        let tokens_before = ctx.total_input_tokens();
        let mut report = CompactionReport { tokens_before, ..Default::default() };

        // Tier 1: always
        self.apply_tier1(ctx, &mut report);

        // Tier 2: if cache window overflow is imminent
        if self.should_apply_tier2(ctx) {
            self.apply_tier2(ctx, &mut report);
        }

        // Tier 3: if context is near full
        if self.should_apply_tier3(ctx) {
            self.apply_tier3(ctx, &mut report).await?;
        }

        report.tokens_after = ctx.total_input_tokens();

        Ok(ProcessorOutput {
            metadata: serde_json::to_value(&report).unwrap_or_default(),
            ..Default::default()
        })
    }
}
```

### 5c. Tier 1 implementation (extends step 77)

Step 77 already dedups file_reads by path. Extend to:

- Drop tool_result content for successive identical bash calls beyond the first.
- Replace tool_result content older than `recent_turns_verbatim × 2` turns with a placeholder, unless the `tool_use_id` is referenced by a later message.

```rust
fn apply_tier1(&self, ctx: &mut WorkingContext, report: &mut CompactionReport) {
    report.tiers_applied.push(CompactionTier::Tier1Deterministic);

    let referenced_ids = collect_referenced_tool_use_ids(&ctx.messages);
    let cutoff_idx = ctx.messages.len().saturating_sub(self.config.recent_turns_verbatim * 4);

    for (idx, msg) in ctx.messages.iter_mut().enumerate() {
        if idx >= cutoff_idx { break; }
        if let Some(tool_use_id) = msg.tool_use_id() {
            if !referenced_ids.contains(&tool_use_id) {
                msg.elide_content("[tool result elided by compaction]");
                report.messages_elided += 1;
            }
        }
    }

    collapse_duplicate_bash_results(&mut ctx.messages, &mut report.messages_elided);
}
```

### 5d. Tier 2 implementation

```rust
fn should_apply_tier2(&self, ctx: &WorkingContext) -> bool {
    ctx.blocks_past_bp4() > self.config.tier2_trigger_blocks_past_bp4
}

fn apply_tier2(&self, ctx: &mut WorkingContext, report: &mut CompactionReport) {
    report.tiers_applied.push(CompactionTier::Tier2CacheAware);

    // Find the oldest-but-safe drop boundary: first message past the cutoff
    // whose tool_use_id is NOT referenced later.
    let drop_until = find_safe_drop_boundary(&ctx.messages, self.config.recent_turns_verbatim);

    if drop_until > 0 {
        let dropped = ctx.messages.drain(0..drop_until).count();
        report.messages_elided += dropped;
        // Insert a single summary-ish placeholder at position 0
        ctx.messages.insert(0, ContextMessage::user(
            format!("[{} earlier messages elided for cache compaction — see session log for full history]", dropped)
        ));
    }
}
```

### 5e. Tier 3 implementation (LLM summarization)

```rust
fn should_apply_tier3(&self, ctx: &WorkingContext) -> bool {
    let limit = self.config.model_context_limit as f64;
    let used = ctx.total_input_tokens() as f64;
    used / limit > self.config.tier3_trigger_fraction
}

async fn apply_tier3(&self, ctx: &mut WorkingContext, report: &mut CompactionReport) -> Result<()> {
    report.tiers_applied.push(CompactionTier::Tier3Summarization);

    let keep_from = ctx.messages.len().saturating_sub(self.config.recent_turns_verbatim * 4);
    if keep_from == 0 { return Ok(()); } // nothing to summarize

    let to_summarize: Vec<ContextMessage> = ctx.messages.drain(0..keep_from).collect();
    let summary = self.call_summarizer(&to_summarize).await?;

    // Insert summary block
    ctx.messages.insert(0, ContextMessage::user(
        format!("<conversation-summary>\n{}\n</conversation-summary>", summary)
    ));

    report.summary_text = Some(summary);
    report.covers_events_up_to = ctx.last_included_event_seq();

    Ok(())
}

async fn call_summarizer(&self, messages: &[ContextMessage]) -> Result<String> {
    // Fixed summarization system prompt (cached across all sessions)
    let system = include_str!("../prompts/summarizer.txt");

    // Rendered conversation (the only dynamic input)
    let user_prompt = render_for_summarization(messages);

    let response = self.llm.complete(CompletionRequest {
        model: self.config.summarizer_model.clone(), // prefer Haiku-class; see step 89
        system: system.to_string(),
        messages: vec![ContextMessage::user(user_prompt)],
        max_tokens: 2048,
        cache_breakpoints: vec![cache_bp_on_system()],
        ..Default::default()
    }).await?;

    Ok(response.text)
}
```

Summarizer system prompt (`moa-brain/src/prompts/summarizer.txt`) is written once and cached forever. Template:

```
You are a summarization assistant. You will be given a sequence of messages
from an AI coding agent's session with a user. Produce a structured summary
that preserves:
- The user's original goal
- Key decisions made during the session
- Files that were read or modified (by path)
- Tools that were called and their high-level outcomes
- Open questions or unresolved issues

Format the summary as a compact markdown document with these sections:
- Goal
- Decisions
- Files touched
- Current state
- Open questions

Do not add preamble. Output only the markdown summary.
```

### 5f. Persist Tier 3 as `Event::Checkpoint`

After Tier 3 produces a summary, emit:

```rust
session_store.emit_event(session_id, Event::Checkpoint {
    summary: report.summary_text.clone().unwrap(),
    covers_events_up_to: report.covers_events_up_to.unwrap(),
    created_at: Utc::now(),
}).await?;
```

On future replay, `apply_event_to_messages` (step 87) sees `Event::Checkpoint` and clears all prior messages in the compiled output, replacing them with the summary block. The `covers_events_up_to` sequence number tells the compiler: "skip reconstructing everything at or before this point."

### 5g. Configuration

```toml
[compaction]
enabled = true
tier2_trigger_blocks_past_bp4 = 14
tier3_trigger_fraction = 0.9
recent_turns_verbatim = 5
max_input_tokens_per_turn = 160000     # 0.8 × 200K
summarizer_model = "claude-haiku-4-5"  # cheaper model for summarization
```

### 5h. Metrics

Add to the turn summary line (step 81):

```
compaction_tier1=true compaction_tier2=false compaction_tier3=false
compaction_tokens_reclaimed=2840 compaction_messages_elided=6
```

### 5i. Tests

- Unit: Tier 1 drops unreferenced tool_results beyond the recent window, preserves referenced ones.
- Unit: Tier 1 is idempotent — applying twice produces the same output.
- Unit: Tier 2 fires only when `blocks_past_bp4 > 14`; drops safe boundary only.
- Unit: Tier 3 with a mock LLM returns a summary and emits an Event::Checkpoint.
- Property: after any tier runs, the resulting messages still satisfy the provider-API invariant (every tool_use has a matching tool_result, or neither is present).
- Integration (extend step 78): run an artificial 25-turn session with deliberately bloated tool outputs. Assert Tier 3 fires by turn 15 (via metric), assert the session completes, assert cache hit rate on turn 20+ is still > 60%.
- Replay test: take a session that triggered Tier 3, delete the snapshot, re-run from events. Confirm the compiled context after replay is byte-identical to the one from the original run (because the Event::Checkpoint is deterministic).

---

## 6. Deliverables

- [ ] `Compactor` pipeline stage with three tiers.
- [ ] Tier 1: extends step 77's dedup, drops orphan tool_results, collapses duplicate bash output.
- [ ] Tier 2: cache-aware trim, triggered by cache-lookback pressure.
- [ ] Tier 3: LLM summarization, persisted as `Event::Checkpoint`.
- [ ] Summarizer prompt file (static, cached across all sessions).
- [ ] Turn summary metrics expose which tiers fired.
- [ ] Step 87's `apply_event_to_messages` handles `Event::Checkpoint` correctly (drop prior, insert summary).
- [ ] Config section for all thresholds.
- [ ] Tests cover each tier in isolation and in combination.
- [ ] Replay determinism test.

---

## 7. Acceptance criteria

1. `cargo test --workspace` green including property + replay tests.
2. A simulated 25-turn session with 20K tokens of bloat per turn completes successfully — no context-limit errors from Anthropic.
3. After Tier 3 fires, the next turn's input tokens drop to roughly `summary_tokens + 5_recent_turns_tokens` — measurably smaller.
4. Step 79 cache hit rate remains above 60% even for post-Tier-3 turns (summary is cached).
5. Replaying a session that triggered Tier 3 from events alone (snapshot deleted) produces byte-identical compiled context to the original run.
6. Compaction metrics appear in the per-turn summary log.
7. Tiers 1 and 2 make zero LLM calls. Tier 3 makes exactly one, using the cheaper summarizer model.

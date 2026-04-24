# 105 — Automated Resolution Detection

## Purpose

Implement automated task resolution scoring that determines whether the agent successfully completed each task segment — without any user-facing feedback buttons. Instead of thumbs up/down, resolution is inferred from five signal classes computed entirely from the event log: tool outcomes, verification results, conversational continuation, agent self-assessment, and async LLM-as-judge.

End state: every completed task segment receives a `resolution` label (`resolved`, `failed`, `abandoned`, `partial`, `unknown`) and a `resolution_confidence` score between 0.0 and 1.0, computed automatically. The resolution signal feeds into skill ranking (prompt 100's `resolution_rate`) and intent learning (prompt 106). No user action is ever required.

## Prerequisites

- Prompt 104 (task segmentation) landed — `task_segments` table exists, `SegmentCompleted` events are emitted.
- Session event log contains `ToolCall`, `ToolResult`, `ToolError`, `BrainResponse`, `UserMessage` events.
- `moa-session` has segment CRUD methods.

## Read before starting

```
cat moa-core/src/types.rs                       # Event enum, TaskSegment
cat moa-session/src/postgres.rs                  # segment CRUD methods
cat moa-orchestrator/src/objects/session.rs       # SessionVoState, SegmentCompleted handling
cat moa-orchestrator/src/turn/runner.rs           # TurnRunner (where tool results flow)
cat moa-brain/src/pipeline/segments.rs            # SegmentTracker
```

## Architecture

### The five signal classes

Resolution is a composite of five independent signals, each producing a score in [0.0, 1.0]:

**Signal 1: Tool outcome analysis (`tool_signal`)**
Examines exit codes and success flags of all tool calls within the segment.

| Pattern | Score |
|---|---|
| All tools succeeded (exit 0, `success: true`) | 0.8 |
| Most tools succeeded, last tool succeeded | 0.7 |
| Mix of success/failure, but agent recovered | 0.5 |
| Last tool call failed | 0.2 |
| All tools failed | 0.1 |
| No tool calls in segment | 0.5 (neutral) |

**Signal 2: Verification detection (`verification_signal`)**
Detects if the agent ran a verification step after completing work (tests, health checks, curl, `echo $?`, build commands). This is the strongest automated signal — modeled on SWE-bench's unit test verification pattern.

| Pattern | Score |
|---|---|
| Agent ran verification command AND it passed | 0.95 |
| Agent ran verification command AND it failed | 0.15 |
| Agent ran a build/compile AND it succeeded | 0.85 |
| Agent wrote code + ran tests + tests passed | 0.95 |
| No verification step detected | 0.5 (neutral) |

Verification commands are detected by pattern matching on tool inputs:
- Test runners: `npm test`, `cargo test`, `pytest`, `go test`, `make test`
- Build commands: `cargo build`, `npm run build`, `make`, `go build`
- Health checks: `curl`, `wget`, status endpoint checks
- Explicit verification: `echo $?`, `git diff --stat`, `ls -la`

**Signal 3: Conversational continuation (`continuation_signal`)**
Examines what the user does AFTER the agent's final response in the segment. This is the strongest implicit signal because it reflects user behavior, not agent behavior.

| Pattern | Score |
|---|---|
| User sends a message starting a NEW task (segment transition) | 0.75 (previous segment likely resolved) |
| User sends a follow-up that BUILDS ON the response | 0.7 |
| User says acknowledgment words: "thanks", "got it", "perfect", "works" | 0.85 |
| User sends correction: "no", "wrong", "that's not what I meant", "try again" | 0.15 |
| User rephrases the same request (detected via embedding similarity) | 0.1 |
| User abandons session (idle timeout >30min) with agent's response as last message | 0.6 |
| User abandons session mid-agent-work (agent was still producing output) | 0.2 |
| Segment is the last in the session (no continuation data yet) | NULL (defer scoring) |

Important: this signal is **retroactive**. When the user sends their next message, we can now score the PREVIOUS segment's continuation signal. This means resolution scoring has two phases: immediate (at segment completion) and deferred (when the next user message arrives).

**Signal 4: Agent self-assessment (`self_assessment_signal`)**
Examines the agent's final response in the segment for completion indicators.

| Pattern | Score |
|---|---|
| Response contains completion language ("Done", "I've completed", "Here's the result", "The changes have been applied") | 0.7 |
| Response contains uncertainty ("I wasn't able to", "I'm not sure if", "This might not work") | 0.3 |
| Response contains explicit failure acknowledgment ("I couldn't", "This failed", "Error:") | 0.15 |
| Response asks for clarification ("Could you clarify", "What do you mean by") | 0.3 |
| Response ends with a question to the user | 0.4 |
| No clear completion or failure signal | 0.5 |

Detection: Use keyword/regex matching for high-confidence patterns. For ambiguous cases, use the same fast model as the QueryRewriter (Haiku-class) with a short prompt: "Did the agent complete the task or not? Respond with: completed, failed, partial, or unclear."

**Signal 5: Structural anomaly detection (`structural_signal`)**
Compares segment metrics against tenant/intent historical baselines.

| Pattern | Score |
|---|---|
| Turn count within 1σ of historical mean for this intent | 0.6 |
| Turn count >2σ above historical mean (agent struggled) | 0.3 |
| Token cost >2σ above historical mean | 0.3 |
| Agent hit turn budget limit (MAX_TURNS) | 0.1 |
| Agent was cancelled by user | 0.0 (→ `abandoned`) |
| Duration within normal range | 0.6 |

This signal is weak in cold-start (no historical data). Weight it at 0.0 until ≥20 segments exist for this tenant+intent pair.

### Composite scoring

```
resolution_score = w1 * tool_signal
                 + w2 * verification_signal
                 + w3 * continuation_signal
                 + w4 * self_assessment_signal
                 + w5 * structural_signal
```

Default weights: `w1=0.20, w2=0.30, w3=0.25, w4=0.15, w5=0.10`

Verification is weighted highest because it's the most objective signal (tests pass/fail is deterministic). Continuation is second because it reflects actual user behavior. Structural is lowest because it's noisy without baseline data.

Any signal that returns NULL (not enough data) is excluded and remaining weights are renormalized.

### Resolution label assignment

| Score range | Label |
|---|---|
| ≥ 0.70 | `resolved` |
| 0.50 – 0.69 | `partial` |
| 0.30 – 0.49 | `unknown` |
| 0.10 – 0.29 | `failed` |
| < 0.10 | `abandoned` |

Special cases that override the composite score:
- User cancelled the session → `abandoned` regardless of score
- Agent hit turn budget → `failed` regardless of score
- Verification command passed → minimum `partial` (score floor 0.50)
- Verification command failed → maximum `partial` (score ceiling 0.49)

## Steps

### 1. Add resolution types to `moa-core/src/types.rs`

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolutionScore {
    pub label: ResolutionLabel,
    pub confidence: f64,
    pub tool_signal: Option<f64>,
    pub verification_signal: Option<f64>,
    pub continuation_signal: Option<f64>,
    pub self_assessment_signal: Option<f64>,
    pub structural_signal: Option<f64>,
    pub scored_at: chrono::DateTime<chrono::Utc>,
    pub scoring_phase: ScoringPhase,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ResolutionLabel {
    Resolved,
    Partial,
    Unknown,
    Failed,
    Abandoned,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScoringPhase {
    Immediate,   // scored at segment completion
    Deferred,    // re-scored when next user message arrives
    Final,       // all signals available, final score
}
```

### 2. Implement individual signal scorers

Create `moa-brain/src/resolution/` module with one file per signal:

**`moa-brain/src/resolution/tool_signal.rs`**
- Input: `Vec<Event>` filtered to `ToolCall`/`ToolResult`/`ToolError` for the segment
- Logic: count successes vs failures, check last-tool status
- Output: `Option<f64>`

**`moa-brain/src/resolution/verification_signal.rs`**
- Input: `Vec<Event>` filtered to `ToolCall` for the segment
- Logic: regex match tool inputs against known verification patterns (`cargo test`, `npm test`, `curl`, etc.)
- If verification detected: check corresponding `ToolResult.success`
- Output: `Option<f64>`
- Include a `VERIFICATION_PATTERNS: &[&str]` constant that's extensible

**`moa-brain/src/resolution/continuation_signal.rs`**
- Input: the NEXT user message after segment completion (if available), plus the agent's last response
- Logic: classify the next message as acknowledgment, correction, new-task, or rephrase
- For rephrase detection: compute embedding similarity between the segment's initial query and the next message. Cosine similarity > 0.85 = rephrase.
- Output: `Option<f64>` (NULL if no next message yet)

**`moa-brain/src/resolution/self_assessment_signal.rs`**
- Input: the agent's last `BrainResponse` text in the segment
- Logic: keyword/regex patterns for completion language, uncertainty, failure acknowledgment
- Optional: fast LLM call for ambiguous cases (with same fail-open/timeout semantics as QueryRewriter)
- Output: `Option<f64>`

**`moa-brain/src/resolution/structural_signal.rs`**
- Input: segment metrics (turn_count, token_cost, duration) + historical baselines for this tenant+intent
- Logic: z-score against historical distribution, flag outliers
- Output: `Option<f64>` (NULL if fewer than 20 historical segments for this tenant+intent)

### 3. Implement composite scorer

Create `moa-brain/src/resolution/scorer.rs`:

```rust
pub struct ResolutionScorer {
    weights: ResolutionWeights,
}

pub struct ResolutionWeights {
    pub tool: f64,           // default: 0.20
    pub verification: f64,   // default: 0.30
    pub continuation: f64,   // default: 0.25
    pub self_assessment: f64, // default: 0.15
    pub structural: f64,     // default: 0.10
}

impl ResolutionScorer {
    pub fn score(
        &self,
        tool: Option<f64>,
        verification: Option<f64>,
        continuation: Option<f64>,
        self_assessment: Option<f64>,
        structural: Option<f64>,
        overrides: &[ResolutionOverride],
    ) -> ResolutionScore {
        // 1. Collect non-null signals with weights
        // 2. Renormalize weights for available signals
        // 3. Compute weighted average
        // 4. Apply overrides (cancellation, turn budget, verification pass/fail)
        // 5. Map to label
    }
}
```

### 4. Wire scoring into segment lifecycle

**Immediate scoring (at segment completion):**
In `SegmentTracker::complete_segment` (called when `is_new_task=true` or session ends):
1. Collect all events for the completing segment
2. Run tool_signal, verification_signal, self_assessment_signal, structural_signal
3. continuation_signal = NULL (no next message yet)
4. Compute composite score with phase=`Immediate`
5. Write to `task_segments.resolution` and `task_segments.resolution_confidence`

**Deferred scoring (when next user message arrives):**
When the QueryRewriter processes a new message and there's a previous segment with `scoring_phase=Immediate`:
1. Run continuation_signal using the new message
2. Re-run composite score with all five signals
3. Update `task_segments.resolution` with phase=`Deferred`

**Session-end scoring:**
When a session completes or times out, score the final segment with continuation_signal based on how the session ended (idle timeout = 0.6, explicit end = 0.65).

### 5. Add scoring config

In `moa-core/src/config.rs`:

```rust
pub struct ResolutionConfig {
    pub enabled: bool,                      // default: true
    pub weights: ResolutionWeights,         // defaults as above
    pub use_llm_self_assessment: bool,      // default: false (start with regex only)
    pub self_assessment_timeout_ms: u64,    // default: 300
    pub rephrase_similarity_threshold: f64, // default: 0.85
    pub structural_min_samples: usize,      // default: 20
    pub idle_timeout_minutes: u64,          // default: 30
}
```

### 6. Add materialized views for skill effectiveness

```sql
-- Skill resolution rates per intent per tenant
CREATE MATERIALIZED VIEW {schema}.skill_resolution_rates AS
SELECT
    t.tenant_id,
    t.intent_label,
    unnest(t.skills_activated) AS skill_name,
    COUNT(*) AS uses,
    AVG(CASE WHEN t.resolution = 'resolved' THEN 1.0
             WHEN t.resolution = 'partial' THEN 0.5
             ELSE 0.0 END) AS resolution_rate,
    AVG(t.token_cost) AS avg_token_cost,
    AVG(t.turn_count) AS avg_turn_count
FROM {schema}.task_segments t
WHERE t.intent_label IS NOT NULL
  AND t.resolution IS NOT NULL
GROUP BY t.tenant_id, t.intent_label, skill_name;

-- Intent transition patterns per tenant
CREATE MATERIALIZED VIEW {schema}.intent_transitions AS
SELECT
    curr.tenant_id,
    prev.intent_label AS from_intent,
    curr.intent_label AS to_intent,
    COUNT(*) AS transition_count,
    AVG(CASE WHEN prev.resolution = 'resolved' THEN 1.0 ELSE 0.0 END) AS from_resolution_rate
FROM {schema}.task_segments curr
JOIN {schema}.task_segments prev ON curr.previous_segment_id = prev.id
WHERE curr.intent_label IS NOT NULL AND prev.intent_label IS NOT NULL
GROUP BY curr.tenant_id, prev.intent_label, curr.intent_label;

-- Historical baselines per tenant+intent for structural signal
CREATE MATERIALIZED VIEW {schema}.segment_baselines AS
SELECT
    tenant_id,
    intent_label,
    COUNT(*) AS sample_count,
    AVG(turn_count) AS avg_turns,
    STDDEV(turn_count) AS stddev_turns,
    AVG(token_cost) AS avg_cost,
    STDDEV(token_cost) AS stddev_cost,
    AVG(EXTRACT(EPOCH FROM (ended_at - started_at))) AS avg_duration_secs,
    STDDEV(EXTRACT(EPOCH FROM (ended_at - started_at))) AS stddev_duration_secs
FROM {schema}.task_segments
WHERE intent_label IS NOT NULL AND ended_at IS NOT NULL
GROUP BY tenant_id, intent_label;
```

Add a Restate scheduled invocation (or cron) to `REFRESH MATERIALIZED VIEW CONCURRENTLY` every 15 minutes.

### 7. Wire resolution rates into SkillInjector ranking

In `moa-brain/src/pipeline/skills.rs` (from prompt 100), update `rank_skills` to use `skill_resolution_rates`:

```rust
// Updated formula:
// score = 0.3 * keyword_overlap
//       + 0.4 * resolution_rate (from materialized view)
//       + 0.2 * use_count_normalized
//       + 0.1 * recency
```

If no resolution data exists for a skill+intent pair, fall back to the prompt-100 formula (without resolution_rate, renormalize remaining weights).

### 8. Tests

- Unit: `tool_signal` — all success → 0.8, all fail → 0.1, mixed → 0.5
- Unit: `verification_signal` — `cargo test` detected and passed → 0.95, detected and failed → 0.15
- Unit: `verification_signal` — no verification commands → 0.5
- Unit: `continuation_signal` — "thanks" → 0.85, "no that's wrong" → 0.15, rephrase → 0.1
- Unit: `self_assessment_signal` — "Done, the file has been updated" → 0.7, "I couldn't find" → 0.15
- Unit: `structural_signal` — turn count within 1σ → 0.6, >2σ → 0.3
- Unit: composite scorer — NULL signals excluded, weights renormalized
- Unit: override — cancellation → `abandoned`, turn budget → `failed`
- Unit: deferred scoring — continuation_signal added on next message, score updated
- Integration: session with verification (test passes) → `resolved` with high confidence
- Integration: session with failed tool → `failed` with low confidence
- Integration: materialized views refresh and contain correct data

## Files to create or modify

- `moa-core/src/types.rs` — add `ResolutionScore`, `ResolutionLabel`, `ScoringPhase`
- `moa-core/src/config.rs` — add `ResolutionConfig`
- `moa-brain/src/resolution/mod.rs` — new module
- `moa-brain/src/resolution/tool_signal.rs` — new
- `moa-brain/src/resolution/verification_signal.rs` — new
- `moa-brain/src/resolution/continuation_signal.rs` — new
- `moa-brain/src/resolution/self_assessment_signal.rs` — new
- `moa-brain/src/resolution/structural_signal.rs` — new
- `moa-brain/src/resolution/scorer.rs` — composite scorer
- `moa-brain/src/pipeline/segments.rs` — wire scoring into segment completion
- `moa-brain/src/pipeline/skills.rs` — use resolution_rate in ranking
- `moa-session/src/schema.rs` — add materialized views migration
- `moa-session/src/postgres.rs` — add view refresh, baseline query methods

## Acceptance criteria

- [ ] `cargo build` succeeds.
- [ ] Segment where agent runs `cargo test` and it passes → `resolved` with confidence ≥ 0.7.
- [ ] Segment where all tool calls fail → `failed` with confidence ≥ 0.7.
- [ ] Segment where user says "thanks" and moves on → `resolved` (deferred scoring).
- [ ] Segment where user rephrases the same question → `failed` (deferred scoring).
- [ ] Cancelled session → all open segments marked `abandoned`.
- [ ] Agent hits turn budget → segment marked `failed`.
- [ ] No user-facing feedback UI is required anywhere.
- [ ] Materialized views contain resolution rate data per skill+intent+tenant.
- [ ] SkillInjector uses resolution_rate in its ranking formula.

## Notes

- **No thumbs up, no thumbs down, no rating.** Resolution is computed entirely from the event log. If the signals are wrong, the composite score absorbs the error through multiple independent signals. Over time, the weights can be tuned per-tenant if needed.
- **Deferred scoring is critical.** The continuation_signal — what the user does next — is the single most reliable implicit signal. It's only available when the NEXT message arrives. The system must support two-phase scoring (immediate at segment end, deferred on next message).
- **Verification detection is the strongest automated signal.** An agent that runs `cargo test` after making code changes and the tests pass has objectively resolved the task. This is the same principle SWE-bench uses (unit test verification). MOA should encourage the brain's system prompt to include verification steps.
- **The structural signal is cold-start-safe.** It returns NULL until 20+ historical segments exist. The composite scorer handles NULL by excluding and renormalizing. New tenants start with tool+verification+continuation+self_assessment signals only.
- **Start with regex-only self-assessment.** The LLM-based self-assessment option (`use_llm_self_assessment`) is off by default. The regex patterns cover 80%+ of cases. Enable the LLM path once the system is stable and the cost is justified.

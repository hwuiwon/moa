# Step 71 — Turn Budget and Loop Drift Detection

_Add a configurable per-session turn limit and semantic loop detection. Prevents the runaway thrashing observed in the 2026-04-15 e2e test (30+ turns with no convergence)._

---

## 1. What this step is about

The brain loop currently runs until the LLM emits `EndTurn`, the user cancels, or the cost budget (Step 65) is hit. There is no turn-count limit and no detection of circular behavior. The 2026-04-15 e2e test showed the brain running 30+ turns — making broken edits, partially fixing them, reformatting unrelated code, and never reaching verification — with no mechanism to stop.

Production agents cluster around 20–25 tool calls as the default limit (Cursor: 25, Windsurf: 20, Cline: 20, CrewAI: 25, LangGraph: 25, OpenAI Agents SDK: 10). MOA should implement:
1. A configurable `max_turns` limit (default: 50 — generous but finite)
2. A semantic loop detector that catches repetitive patterns earlier

---

## 2. Files to read

- **`moa-core/src/config.rs`** — Add a new `SessionLimitsConfig` section. Add `max_turns` and `loop_detection_threshold` here.
- **`moa-core/src/error.rs`** — Consider whether `TurnLimitReached` needs its own variant or can use the existing `Warning` event path.
- **`moa-orchestrator/src/local.rs`** — The `run_session_loop` / brain turn loop. Turn counting and limit enforcement go here.
- **`moa-runtime/src/local.rs`** — `LocalChatRuntime` turn loop. Same enforcement.
- **`moa-brain/src/turn.rs`** — Turn result types. May need a new `TurnResult` variant or handle via the existing error path.
- **`moa-core/src/types/event.rs`** — `Event` enum. The `Warning` variant is likely sufficient; no new event type needed.

---

## 3. Goal

After this step:
1. A per-session turn counter increments after each brain turn
2. When `turn_count >= max_turns`, the brain stops gracefully with a summary of what it accomplished
3. A loop detector flags repetitive tool call patterns and triggers an early stop
4. The user sees a clear message: "Session paused after 50 turns. Use /resume to continue."
5. The session status is set to `Paused` (not `Failed` or `Completed`), allowing resumption

---

## 4. Rules

- **Default `max_turns = 50`.** This is generous — Cursor uses 25, most frameworks use 20. MOA's design allows resumption, so 50 is a reasonable ceiling. Configurable in `config.toml`.
- **Turn = one complete brain turn** (LLM call + tool execution cycle). A turn that produces 3 tool calls counts as 1 turn, not 3. This matches the industry convention.
- **Loop detection uses fingerprinting.** Hash the last N `(tool_name, truncated_output_prefix)` tuples. If 3 consecutive turns produce identical fingerprints, trigger a loop stop. This catches the "read file → edit file → get same error → read file again" cycle without false positives on legitimately repetitive work.
- **Soft stop, not hard stop.** When the limit is reached:
  1. Emit a `Warning` event with a summary
  2. Set session status to `Paused`
  3. The user can resume with `/resume` or `moa resume`
  4. Upon resumption, the turn counter resets (the user explicitly chose to continue)
- **Loop detection is separate from turn limit.** Loop detection can fire at turn 5 if the pattern is clearly circular. Turn limit fires at max_turns regardless.
- **`max_turns = 0` means unlimited.** Same convention as `daily_workspace_cents`.

---

## 5. Tasks

### 5a. Add session limits config

```rust
// In config.rs
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionLimitsConfig {
    /// Maximum turns per session before pausing. 0 = unlimited.
    pub max_turns: u32,
    /// Number of identical consecutive turn fingerprints that triggers a loop stop.
    pub loop_detection_threshold: u32,
}

impl Default for SessionLimitsConfig {
    fn default() -> Self {
        Self {
            max_turns: 50,
            loop_detection_threshold: 3,
        }
    }
}
```

Add `pub session_limits: SessionLimitsConfig` to `MoaConfig`.

Config format:
```toml
[session_limits]
max_turns = 50
loop_detection_threshold = 3
```

### 5b. Add loop fingerprinting

Create `moa-brain/src/loop_detector.rs`:

```rust
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::collections::hash_map::DefaultHasher;

const OUTPUT_PREFIX_LEN: usize = 200;

/// Tracks recent turn fingerprints to detect circular agent behavior.
#[derive(Debug, Clone)]
pub struct LoopDetector {
    threshold: u32,
    recent_fingerprints: VecDeque<u64>,
}

impl LoopDetector {
    pub fn new(threshold: u32) -> Self {
        Self {
            threshold,
            recent_fingerprints: VecDeque::with_capacity(threshold as usize + 1),
        }
    }

    /// Records a turn's tool calls and returns true if a loop is detected.
    ///
    /// Each entry in `tool_calls` is `(tool_name, output_or_result_text)`.
    pub fn record_turn(&mut self, tool_calls: &[(String, String)]) -> bool {
        if self.threshold == 0 {
            return false; // disabled
        }

        let fingerprint = self.fingerprint(tool_calls);
        self.recent_fingerprints.push_back(fingerprint);

        // Keep only `threshold` most recent
        while self.recent_fingerprints.len() > self.threshold as usize {
            self.recent_fingerprints.pop_front();
        }

        // Check if all entries are identical
        if self.recent_fingerprints.len() < self.threshold as usize {
            return false;
        }

        let first = self.recent_fingerprints[0];
        self.recent_fingerprints.iter().all(|fp| *fp == first)
    }

    /// Resets the detector (called on session resume).
    pub fn reset(&mut self) {
        self.recent_fingerprints.clear();
    }

    fn fingerprint(&self, tool_calls: &[(String, String)]) -> u64 {
        let mut hasher = DefaultHasher::new();
        for (name, output) in tool_calls {
            name.hash(&mut hasher);
            // Use first N chars of output to avoid hashing huge payloads
            let prefix = if output.len() > OUTPUT_PREFIX_LEN {
                &output[..OUTPUT_PREFIX_LEN]
            } else {
                output.as_str()
            };
            prefix.hash(&mut hasher);
        }
        hasher.finish()
    }
}
```

### 5c. Integrate into the brain turn loop

In the orchestrator/runtime turn loop (both `moa-orchestrator/src/local.rs` and `moa-runtime/src/local.rs`):

```rust
let mut turn_count: u32 = 0;
let mut loop_detector = LoopDetector::new(
    config.session_limits.loop_detection_threshold
);

loop {
    // Check turn limit before each turn
    if config.session_limits.max_turns > 0
        && turn_count >= config.session_limits.max_turns
    {
        let message = format!(
            "Session paused after {} turns. Use /resume to continue.",
            turn_count
        );
        store.emit_event(session_id, Event::Warning {
            message: message.clone(),
        }).await?;
        on_runtime_event(RuntimeEvent::Notice(message));
        store.update_status(session_id, SessionStatus::Paused).await?;
        break;
    }

    // Run the turn
    let turn_result = run_brain_turn(/* ... */).await?;
    turn_count += 1;

    // Feed tool calls to loop detector
    if let Some(tool_calls) = turn_result.tool_call_summaries() {
        if loop_detector.record_turn(&tool_calls) {
            let message = format!(
                "Loop detected: {} consecutive turns with identical tool calls. \
                 Session paused. Use /resume to continue.",
                config.session_limits.loop_detection_threshold
            );
            store.emit_event(session_id, Event::Warning {
                message: message.clone(),
            }).await?;
            on_runtime_event(RuntimeEvent::Notice(message));
            store.update_status(session_id, SessionStatus::Paused).await?;
            break;
        }
    }

    // ... existing match on turn_result ...
}
```

### 5d. Extract tool call summaries from turn results

The turn result needs a method to extract `(tool_name, output_prefix)` tuples for the loop detector. Add a helper that collects this from the events emitted during the turn. The exact shape depends on whether tool results are captured in the `TurnResult` or in the session store — check the current implementation and extract from whichever is available.

If tool calls are in the session store, query the last turn's `ToolCall`/`ToolResult` events:

```rust
fn collect_turn_tool_summaries(events: &[EventRecord]) -> Vec<(String, String)> {
    let mut summaries = Vec::new();
    let mut tool_outputs: std::collections::HashMap<uuid::Uuid, String> = std::collections::HashMap::new();

    // Collect tool results
    for event in events {
        if let Event::ToolResult { tool_id, output, .. } = &event.event {
            tool_outputs.insert(*tool_id, output.chars().take(200).collect());
        }
    }

    // Match tool calls with their results
    for event in events {
        if let Event::ToolCall { tool_id, tool_name, .. } = &event.event {
            let output = tool_outputs.get(tool_id).cloned().unwrap_or_default();
            summaries.push((tool_name.clone(), output));
        }
    }

    summaries
}
```

### 5e. Reset on resume

When a session is resumed (`/resume` or `moa resume`):
- Reset `turn_count` to 0
- Call `loop_detector.reset()`
- This is intentional: the user explicitly chose to continue, so they get another full budget

### 5f. Update `sample-config.toml`

Add:
```toml
[session_limits]
max_turns = 50               # 0 = unlimited
loop_detection_threshold = 3  # 0 = disabled
```

### 5g. Add tests

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loop_detector_triggers_after_threshold() {
        let mut detector = LoopDetector::new(3);
        let calls = vec![
            ("file_read".to_string(), "contents of views.py...".to_string()),
        ];
        assert!(!detector.record_turn(&calls));
        assert!(!detector.record_turn(&calls));
        assert!(detector.record_turn(&calls)); // 3rd identical → loop
    }

    #[test]
    fn loop_detector_does_not_trigger_on_varied_calls() {
        let mut detector = LoopDetector::new(3);
        assert!(!detector.record_turn(&[("file_read".into(), "a".into())]));
        assert!(!detector.record_turn(&[("bash".into(), "b".into())]));
        assert!(!detector.record_turn(&[("file_write".into(), "c".into())]));
    }

    #[test]
    fn loop_detector_disabled_at_zero_threshold() {
        let mut detector = LoopDetector::new(0);
        let calls = vec![("file_read".into(), "same".into())];
        for _ in 0..10 {
            assert!(!detector.record_turn(&calls));
        }
    }

    #[test]
    fn loop_detector_resets() {
        let mut detector = LoopDetector::new(3);
        let calls = vec![("bash".into(), "output".into())];
        detector.record_turn(&calls);
        detector.record_turn(&calls);
        detector.reset();
        assert!(!detector.record_turn(&calls)); // only 1 after reset
    }

    #[test]
    fn loop_detector_sliding_window() {
        let mut detector = LoopDetector::new(3);
        let a = vec![("bash".into(), "output_a".into())];
        let b = vec![("bash".into(), "output_b".into())];
        detector.record_turn(&a);
        detector.record_turn(&a);
        detector.record_turn(&b); // breaks the streak
        assert!(!detector.record_turn(&b));
        assert!(!detector.record_turn(&b)); // only 2 consecutive b's in window
    }
}
```

---

## 6. Deliverables

- [ ] `moa-core/src/config.rs` — `SessionLimitsConfig` added to `MoaConfig`
- [ ] `moa-brain/src/loop_detector.rs` (new) — `LoopDetector` struct
- [ ] `moa-brain/src/lib.rs` — Export `loop_detector` module
- [ ] `moa-orchestrator/src/local.rs` — Turn counter and loop detection in brain loop
- [ ] `moa-runtime/src/local.rs` — Same enforcement in `LocalChatRuntime`
- [ ] Tests for `LoopDetector` (trigger, no-trigger, disabled, reset, sliding window)
- [ ] `docs/sample-config.toml` — `[session_limits]` section added

---

## 7. Acceptance criteria

1. Setting `max_turns = 5` and running a session that would take 10 turns → session pauses after 5 with "Session paused after 5 turns."
2. Setting `max_turns = 0` → no turn limit enforced.
3. Three consecutive identical tool call turns trigger loop detection and pause the session.
4. Resuming a paused session resets both the turn counter and loop detector.
5. Paused sessions have status `Paused`, not `Failed` or `Completed`.
6. `cargo test -p moa-brain -p moa-orchestrator` passes.

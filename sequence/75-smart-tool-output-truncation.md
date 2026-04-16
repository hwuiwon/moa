# Step 75 — Smart Tool Output Truncation with Head+Tail Preservation

_Replace the naive character-limit truncation with intelligent head+tail preservation that keeps the most useful parts of large tool outputs. Reduce context pollution from oversized bash and file_read results._

---

## 1. What this step is about

MOA currently truncates tool output for history replay at `MAX_TOOL_RESULT_REPLAY_CHARS = 100_000` characters — approximately 25,000 tokens. This is far too generous. Codex CLI caps at **256 lines or 10 KiB** and is actively debating moving to token-based limits (GitHub issue #6426). Manus reports that tool responses consume **67.6%** of total token usage, dwarfing system prompts at 3.4%.

The current truncation also uses a simple prefix cut: keep the first N characters, discard the rest. This loses critical information that typically appears at the **end** of output — build errors, test results, command exit codes, and summary lines. The industry standard is head+tail truncation: keep the first N lines and the last M lines, replacing the middle with `[... X lines omitted ...]`.

---

## 2. Files to read

- **`moa-brain/src/pipeline/history.rs`** — `MAX_TOOL_RESULT_REPLAY_CHARS`, `truncate_tool_result_text()`, `replayable_tool_content_blocks()`. This is where replay truncation happens.
- **`moa-hands/src/tools/bash.rs`** — Bash output is the primary source of oversized tool results.
- **`moa-core/src/config.rs`** — Add configurable truncation limits.
- **`moa-core/src/types/tool.rs`** — `ToolOutput` struct. May need a `truncated` flag.

---

## 3. Goal

After this step:
1. Tool output truncation uses head+tail preservation instead of prefix-only
2. The default limit is reduced from 100K chars to **20K chars** (~5K tokens)
3. Bash output specifically gets a line-based limit (200 lines) in addition to the char limit
4. Truncated output clearly indicates what was omitted: `[... 847 lines omitted ...]`
5. The limits are configurable in `config.toml`

---

## 4. Rules

- **Head+tail split: 40%/60%.** Keep 40% of the budget for the head (command output start, file headers) and 60% for the tail (errors, summaries, exit codes). Tail is more valuable in practice.
- **Line-based truncation for bash.** Bash output is line-oriented. Apply line limits (default: 200 lines) before character limits. This prevents 256 very long lines from consuming 100K characters.
- **Character limit for non-bash tools.** File reads and other tools use character limits (default: 20K chars).
- **The omission marker must include the count.** `[... 847 lines omitted ...]` or `[... ~15000 chars omitted ...]`. This helps the brain decide whether to re-read the full output.
- **Truncation happens at two levels:**
  1. **Immediate (tool execution):** The `ToolOutput` returned by the tool is truncated before being stored as an event. This prevents the session database from growing unboundedly.
  2. **Replay (history compilation):** The `HistoryCompiler` applies a second, tighter limit when replaying events into context. This is the existing `MAX_TOOL_RESULT_REPLAY_CHARS` path.
- **The `ToolOutput` gains a `truncated: bool` field** so the brain knows the output was cut and can re-read if needed.

---

## 5. Tasks

### 5a. Add truncation config

```rust
// In config.rs, add to a new or existing section
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolOutputConfig {
    /// Maximum characters for non-bash tool output in history replay.
    pub max_replay_chars: usize,
    /// Maximum lines for bash tool output.
    pub max_bash_lines: usize,
    /// Head/tail split ratio (0.0–1.0, fraction allocated to head).
    pub head_ratio: f64,
}

impl Default for ToolOutputConfig {
    fn default() -> Self {
        Self {
            max_replay_chars: 20_000,
            max_bash_lines: 200,
            head_ratio: 0.4,
        }
    }
}
```

### 5b. Implement head+tail truncation

Create `moa-core/src/truncation.rs`:

```rust
/// Truncates text using head+tail preservation.
pub fn truncate_head_tail(text: &str, max_chars: usize, head_ratio: f64) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_string(), false);
    }

    let head_budget = (max_chars as f64 * head_ratio) as usize;
    let tail_budget = max_chars - head_budget;

    let head: String = text.chars().take(head_budget).collect();
    let tail: String = text.chars().rev().take(tail_budget).collect::<String>().chars().rev().collect();

    // Find clean line boundaries
    let head_end = head.rfind('\n').map(|i| i + 1).unwrap_or(head.len());
    let tail_start_in_original = text.len() - tail.len();
    let tail_clean_start = tail.find('\n').map(|i| i + 1).unwrap_or(0);

    let head_clean = &head[..head_end];
    let tail_clean = &tail[tail_clean_start..];

    let omitted_chars = text.chars().count() - head_clean.chars().count() - tail_clean.chars().count();

    let result = format!(
        "{}\n[... ~{} chars omitted ...]\n{}",
        head_clean.trim_end(),
        omitted_chars,
        tail_clean.trim_start()
    );

    (result, true)
}

/// Truncates text by line count using head+tail preservation.
pub fn truncate_head_tail_lines(text: &str, max_lines: usize, head_ratio: f64) -> (String, bool) {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max_lines {
        return (text.to_string(), false);
    }

    let head_lines = (max_lines as f64 * head_ratio) as usize;
    let tail_lines = max_lines - head_lines;
    let omitted = lines.len() - head_lines - tail_lines;

    let mut result = lines[..head_lines].join("\n");
    result.push_str(&format!("\n\n[... {} lines omitted ...]\n\n", omitted));
    result.push_str(&lines[lines.len() - tail_lines..].join("\n"));

    (result, true)
}
```

### 5c. Apply at tool execution time for bash

In `bash.rs`, apply line-based truncation to the combined stdout+stderr before returning:

```rust
let (truncated_output, was_truncated) = truncate_head_tail_lines(
    &combined_output,
    config.tool_output.max_bash_lines,
    config.tool_output.head_ratio,
);
```

### 5d. Update history replay truncation

In `history.rs`, replace the current `truncate_tool_result_text` with the head+tail version:

```rust
fn truncate_tool_result_text(text: &str, max_chars: usize, head_ratio: f64) -> String {
    let (truncated, _) = moa_core::truncation::truncate_head_tail(text, max_chars, head_ratio);
    truncated
}
```

Reduce `MAX_TOOL_RESULT_REPLAY_CHARS` to use the config value (default 20K).

### 5e. Add `truncated` field to `ToolOutput`

```rust
pub struct ToolOutput {
    pub content: Vec<ToolContent>,
    pub is_error: bool,
    pub structured: Option<serde_json::Value>,
    pub duration: Duration,
    pub truncated: bool,  // NEW
}
```

### 5f. Add tests

```rust
#[test]
fn head_tail_preserves_both_ends() {
    let input = (1..=100).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
    let (result, truncated) = truncate_head_tail_lines(&input, 20, 0.4);
    assert!(truncated);
    assert!(result.contains("line 1"));    // head preserved
    assert!(result.contains("line 100"));  // tail preserved
    assert!(result.contains("[... 80 lines omitted ...]"));
    assert!(!result.contains("line 50"));  // middle omitted
}

#[test]
fn small_output_not_truncated() {
    let input = "hello\nworld\n";
    let (result, truncated) = truncate_head_tail_lines(input, 200, 0.4);
    assert!(!truncated);
    assert_eq!(result, input);
}
```

---

## 6. Deliverables

- [ ] `moa-core/src/truncation.rs` (new) — Head+tail truncation utilities
- [ ] `moa-core/src/config.rs` — `ToolOutputConfig` section
- [ ] `moa-core/src/types/tool.rs` — `truncated` field on `ToolOutput`
- [ ] `moa-brain/src/pipeline/history.rs` — Updated replay truncation
- [ ] `moa-hands/src/tools/bash.rs` — Line-based truncation at execution time
- [ ] Tests for head+tail char and line truncation

---

## 7. Acceptance criteria

1. A bash command producing 1000 lines of output is truncated to 200 lines with head+tail preservation.
2. The tail (last 120 lines by default) is preserved, keeping error messages and test results visible.
3. A truncation marker `[... N lines omitted ...]` appears in the output.
4. History replay uses 20K chars instead of 100K chars.
5. Small outputs (<200 lines, <20K chars) are not truncated.
6. `cargo test -p moa-core -p moa-hands -p moa-brain` passes.

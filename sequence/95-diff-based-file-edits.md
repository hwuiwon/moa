# Step 95 — Diff-Based Output for File Edits

_When the agent writes or edits a file, emit a unified diff instead of echoing the full file content back. Output tokens cost 5× input tokens ($15/MTok vs $3/MTok for Sonnet). A diff that's 30% of the full file saves ~70% of output tokens on file-edit turns._

---

## 1. What this step is about

Today `str_replace` and `file_write` tools return the full affected region (or a success message with the changed line range). But the LLM's assistant response that called the tool already contains the `new_str` — the full file content in the tool result is redundant context. Roo Code's diff-based editing pattern demonstrated ~30% output token savings.

This step changes tool results for file-mutation tools to return a compact unified diff instead of full content.

---

## 2. Files to read

- `moa-hands/src/tools/str_replace.rs` — current return format.
- `moa-hands/src/tools/file_write.rs` — current return format.
- `moa-core/src/truncation.rs` — may need diff-aware truncation.
- `similar` crate (already in workspace deps from TUI diff view) — unified diff generation.

---

## 3. Goal

1. `str_replace` tool result returns a unified diff showing only the changed hunks, with 3 lines of context.
2. `file_write` tool result returns a unified diff of the old vs new file (or `[new file created: {path}, {line_count} lines]` for new files).
3. Diff output is capped at step 94's per-tool budget.
4. The full file content is NOT in the tool result. The agent can re-read the file if needed.

---

## 4. Rules

- **Diffs use unified format.** `--- a/{path}\n+++ b/{path}\n@@ ... @@`. Standard, LLM-parseable.
- **Context lines = 3.** Matches `git diff` default. Enough for the LLM to orient.
- **New file creation: no diff.** Just `[new file created: {path}, {N} lines]`. Diffing against nothing is noise.
- **Binary files: no diff.** Return `[binary file written: {path}, {size} bytes]`.
- **Keep the before-snapshot in memory, not on disk.** Read the file before applying the edit; compute diff in-memory; return diff. Don't write temporary files.

---

## 5. Tasks

### 5a. Capture pre-edit content

In `str_replace` handler, before applying the edit:
```rust
let before = tokio::fs::read_to_string(&path).await?;
// ... apply str_replace ...
let after = tokio::fs::read_to_string(&path).await?;
let diff = compute_unified_diff(&path, &before, &after, 3);
```

### 5b. `compute_unified_diff` helper

```rust
// moa-core/src/diff.rs or moa-hands/src/tools/diff.rs
use similar::{TextDiff, ChangeTag};

pub fn compute_unified_diff(path: &str, before: &str, after: &str, context: usize) -> String {
    let diff = TextDiff::from_lines(before, after);
    diff.unified_diff()
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .context_radius(context)
        .to_string()
}
```

### 5c. Wire into tool results

- `str_replace`: return the diff string.
- `file_write` (overwrite): capture old content before write, return diff.
- `file_write` (new file): return `[new file created: ...]`.

### 5d. Tests

- `str_replace` changing 2 lines in a 100-line file: diff is ~10 lines, not 100.
- `file_write` overwriting a file: diff shows old vs new.
- New file creation: no diff, just the creation notice.
- Token savings measurement: compare `estimated_tokens(diff_result)` vs `estimated_tokens(full_file_result)` on a realistic 500-line file edit.

---

## 6. Deliverables

- [ ] `compute_unified_diff` utility function.
- [ ] `str_replace` returns unified diff.
- [ ] `file_write` returns unified diff (overwrite) or creation notice (new file).
- [ ] Tests verifying format and token savings.

---

## 7. Acceptance criteria

1. A `str_replace` editing 3 lines in a 500-line file returns a tool result of ~15 lines (hunks + context), not 500.
2. The diff is valid unified format parseable by `git apply`.
3. Token count of the diff result is ≤30% of the full-file result for edits affecting <10% of the file.
4. New file creations return a one-line notice, not a diff.
5. `cargo test --workspace` green.

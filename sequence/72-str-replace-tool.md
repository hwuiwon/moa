# Step 72 — Surgical File Editing via `str_replace` Tool

_Replace the full-file-overwrite `file_write` with a surgical `str_replace`-based editing tool. Eliminates the dominant token waste and error source observed in the 2026-04-15 e2e test._

---

## 1. What this step is about

MOA's only file modification tool is `file_write`, which requires the brain to output the **entire file contents** as the `content` parameter. For a 19,000-line file like `server/core/views.py` in the applied test, this means:
- ~76,000 output tokens per edit (at ~4 chars/token)
- The LLM must reproduce the entire file perfectly, including lines it didn't change
- Any reproduction error corrupts unrelated code (the "collateral churn" problem)
- The approval diff shows the entire file, making review impossible

SWE-bench Verified data shows **77.6% of real-world fixes touch only one function, with a median of 4 lines changed**. Full-file rewrites are 90%+ wasteful for the dominant case.

Claude Code uses `str_replace_editor` (5 commands: `view`, `str_replace`, `create`, `insert`, `undo_edit`). Codex CLI uses `apply_patch` with a V4A diff format. Aider supports 7 edit formats, with SEARCH/REPLACE performing best on the Diff-XYZ benchmark.

MOA should adopt the `str_replace` approach: the brain outputs only the `old_str` to find and the `new_str` to replace it with. This is the simplest, most robust, and most token-efficient pattern.

---

## 2. Files to read

- **`moa-hands/src/tools/file_write.rs`** — Current full-file overwrite. Will be kept for `create` operations but demoted for edits.
- **`moa-hands/src/tools/file_read.rs`** — `resolve_sandbox_path` reused by the new tool.
- **`moa-hands/src/tools/mod.rs`** — Module registry. Add the new tool here.
- **`moa-hands/src/router/registration.rs`** — `ToolRegistry::default_local()`. Register the new tool.
- **`moa-hands/src/local.rs`** — `LocalHandProvider::execute()` match arm routing. Add `str_replace` dispatch.
- **`moa-core/src/types/policy.rs`** — `ToolDiffStrategy`, `ToolInputShape`. May need a new variant.
- **`moa-brain/src/pipeline/identity.rs`** — Update identity prompt to reference `str_replace` instead of `file_write` for edits.

---

## 3. Goal

After this step:
1. A new `str_replace` hand tool is registered and available in the default loadout
2. The tool takes `path`, `old_str`, and `new_str` parameters
3. `old_str` must match exactly one location in the file (error if 0 or >1 matches)
4. The tool returns a confirmation with the line range that was modified
5. The identity prompt directs the brain to prefer `str_replace` for edits, `file_write` for new files
6. Approval diffs show only the changed region, not the entire file

---

## 4. Rules

- **Exact match required.** `old_str` must appear exactly once in the file, including whitespace and indentation. If it appears 0 or >1 times, return a clear error with the match count and surrounding context for disambiguation. Do NOT silently pick the first match.
- **No multi-edit batching.** Each `str_replace` call modifies one location. The brain makes separate calls for multiple edits. This matches Claude Code's approach and avoids ordering ambiguity.
- **Empty `new_str` means deletion.** If `new_str` is empty or omitted, the matched region is deleted.
- **Empty `old_str` means insertion.** If `old_str` is empty, `new_str` is inserted at the specified `insert_after_line` (required when `old_str` is empty).
- **The tool creates parent directories** if the file doesn't exist yet and `old_str` is empty (effectively a create operation).
- **Keep `file_write` for new file creation.** Don't remove it. The brain should use `file_write` when creating a file from scratch, and `str_replace` when editing an existing file.
- **Approval policy: `RequireApproval` (write policy).** Same as `file_write`. The diff shown to the user should be the surgical change, not the full file.
- **`ToolDiffStrategy::StrReplace`** — new variant that generates a diff from only the `old_str`→`new_str` change rather than the full file comparison used by `FileWrite`.

---

## 5. Tasks

### 5a. Create `moa-hands/src/tools/str_replace.rs`

```rust
use std::path::Path;
use std::time::Duration;

use moa_core::{MoaError, Result, ToolOutput};
use serde::Deserialize;
use tokio::fs;

use crate::tools::file_read::resolve_sandbox_path;

const MAX_CONTEXT_LINES: usize = 4;

pub async fn execute(sandbox_dir: &Path, input: &str) -> Result<ToolOutput> {
    let params: StrReplaceInput = serde_json::from_str(input)?;
    let path = resolve_sandbox_path(sandbox_dir, &params.path)?;

    // Handle creation case (empty old_str)
    if params.old_str.is_empty() {
        if let Some(line) = params.insert_after_line {
            return insert_at_line(&path, line, &params.new_str).await;
        }
        // No old_str and no insert_after_line: create new file
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&path, &params.new_str).await?;
        return Ok(ToolOutput::text(
            format!("created {}", display_path(sandbox_dir, &path)),
            Duration::default(),
        ));
    }

    let content = fs::read_to_string(&path).await.map_err(|e| {
        MoaError::ToolError(format!("cannot read {}: {e}", params.path))
    })?;

    // Find all matches
    let matches: Vec<usize> = content.match_indices(&params.old_str)
        .map(|(idx, _)| idx)
        .collect();

    match matches.len() {
        0 => Err(MoaError::ToolError(format!(
            "str_replace failed: old_str not found in {}. \
             Make sure the string matches exactly, including whitespace and indentation.",
            params.path,
        ))),
        1 => {
            let match_start = matches[0];
            let before = &content[..match_start];
            let after = &content[match_start + params.old_str.len()..];
            let new_content = format!("{before}{}{after}", params.new_str);

            let start_line = before.lines().count();
            let old_line_count = params.old_str.lines().count();
            let new_line_count = params.new_str.lines().count();

            fs::write(&path, &new_content).await?;

            Ok(ToolOutput::text(
                format!(
                    "replaced {} lines with {} lines in {} (starting at line {})",
                    old_line_count,
                    new_line_count,
                    display_path(sandbox_dir, &path),
                    start_line + 1,
                ),
                Duration::default(),
            ))
        }
        n => {
            // Multiple matches: return context around each for disambiguation
            let mut hints = String::new();
            for (i, &pos) in matches.iter().take(5).enumerate() {
                let line_num = content[..pos].lines().count() + 1;
                hints.push_str(&format!("  match {}: line {}\n", i + 1, line_num));
            }
            if n > 5 {
                hints.push_str(&format!("  ... and {} more matches\n", n - 5));
            }
            Err(MoaError::ToolError(format!(
                "str_replace failed: old_str found {n} times in {}. \
                 Include more surrounding context to make the match unique.\n{hints}",
                params.path,
            )))
        }
    }
}

async fn insert_at_line(path: &Path, after_line: usize, content: &str) -> Result<ToolOutput> {
    let file_content = fs::read_to_string(path).await?;
    let mut lines: Vec<&str> = file_content.lines().collect();
    let insert_pos = after_line.min(lines.len());
    let new_lines: Vec<&str> = content.lines().collect();
    let inserted_count = new_lines.len();

    for (i, line) in new_lines.into_iter().enumerate() {
        lines.insert(insert_pos + i, line);
    }

    let result = lines.join("\n");
    // Preserve trailing newline if original had one
    let result = if file_content.ends_with('\n') {
        format!("{result}\n")
    } else {
        result
    };

    fs::write(path, &result).await?;
    Ok(ToolOutput::text(
        format!("inserted {inserted_count} lines after line {after_line}"),
        Duration::default(),
    ))
}

fn display_path(sandbox_dir: &Path, path: &Path) -> String {
    path.strip_prefix(sandbox_dir)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[derive(Debug, Deserialize)]
struct StrReplaceInput {
    path: String,
    #[serde(default)]
    old_str: String,
    #[serde(default)]
    new_str: String,
    insert_after_line: Option<usize>,
}
```

### 5b. Add Docker-backed execution path

Create `execute_docker` in `str_replace.rs` that uses `docker exec` to read/write within the container sandbox, following the same pattern as `file_write::execute_docker`.

### 5c. Register the tool in `registration.rs`

```rust
registry.register_hand(
    "str_replace",
    "Replace a unique string in a file with another string. old_str must match exactly once in the file. Use this for all code edits instead of file_write.",
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Relative path within the workspace root." },
            "old_str": { "type": "string", "description": "Exact string to find and replace. Must match exactly once. Include enough context (indentation, surrounding lines) to make the match unique." },
            "new_str": { "type": "string", "description": "Replacement string. Empty to delete the matched region." },
            "insert_after_line": { "type": "integer", "description": "Line number to insert after when old_str is empty (insertion mode)." }
        },
        "required": ["path"],
        "additionalProperties": false
    }),
    write_tool_policy(ToolInputShape::Path, ToolDiffStrategy::StrReplace),
);
```

Add `str_replace` to the `default_loadout` vector.

### 5d. Add `ToolDiffStrategy::StrReplace` variant

In `moa-core/src/types/policy.rs`, add:

```rust
pub enum ToolDiffStrategy {
    None,
    FileWrite,
    StrReplace,  // NEW: diff only the old_str→new_str region
}
```

Update `approval_diffs_for` in `normalization.rs` to handle `StrReplace` — show a diff of only the replaced region with a few lines of surrounding context, not the entire file.

### 5e. Route execution in `LocalHandProvider`

Add the `str_replace` match arm in `execute()` / `execute_docker()`, following the existing patterns for `file_write`.

### 5f. Update identity prompt

In `identity.rs`, change the existing guidance:

```
- Prefer the str_replace tool for editing existing files. It replaces one
  unique string match per call — include enough surrounding context
  (indentation, nearby lines) to make old_str match exactly once. Use
  file_write only when creating new files from scratch.
```

### 5g. Add tests

```rust
#[tokio::test]
async fn str_replace_single_match() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("test.py"), "def foo():\n    return 1\n").await.unwrap();
    let output = execute(dir.path(), r#"{"path":"test.py","old_str":"return 1","new_str":"return 42"}"#).await.unwrap();
    let content = fs::read_to_string(dir.path().join("test.py")).await.unwrap();
    assert!(content.contains("return 42"));
    assert!(!content.contains("return 1"));
}

#[tokio::test]
async fn str_replace_no_match_errors() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("test.py"), "def foo():\n    return 1\n").await.unwrap();
    let result = execute(dir.path(), r#"{"path":"test.py","old_str":"return 999","new_str":"x"}"#).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("not found"));
}

#[tokio::test]
async fn str_replace_multiple_matches_errors() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("test.py"), "x = 1\nx = 2\nx = 3\n").await.unwrap();
    let result = execute(dir.path(), r#"{"path":"test.py","old_str":"x = ","new_str":"y = "}"#).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("3 times"));
}

#[tokio::test]
async fn str_replace_deletion() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("test.py"), "line1\ndelete_me\nline3\n").await.unwrap();
    execute(dir.path(), r#"{"path":"test.py","old_str":"delete_me\n","new_str":""}"#).await.unwrap();
    let content = fs::read_to_string(dir.path().join("test.py")).await.unwrap();
    assert!(!content.contains("delete_me"));
    assert!(content.contains("line1\nline3"));
}
```

---

## 6. Deliverables

- [ ] `moa-hands/src/tools/str_replace.rs` (new) — Core implementation
- [ ] `moa-hands/src/tools/mod.rs` — Export the new module
- [ ] `moa-hands/src/router/registration.rs` — Register tool, add to default loadout
- [ ] `moa-hands/src/local.rs` — Execution routing
- [ ] `moa-core/src/types/policy.rs` — `ToolDiffStrategy::StrReplace` variant
- [ ] `moa-hands/src/router/normalization.rs` — Surgical diff for approval display
- [ ] `moa-brain/src/pipeline/identity.rs` — Update prompt to prefer `str_replace`
- [ ] Tests covering single match, no match, multiple matches, deletion, insertion, and Docker path

---

## 7. Acceptance criteria

1. `str_replace` with a unique `old_str` modifies only the matched region, leaving the rest of the file intact.
2. `str_replace` with 0 matches returns a clear error message.
3. `str_replace` with >1 matches returns an error with line numbers for each match.
4. The approval diff shows only the changed region, not the entire file.
5. The identity prompt references `str_replace` as the preferred editing tool.
6. `file_write` still works for new file creation.
7. `cargo test -p moa-hands` passes.

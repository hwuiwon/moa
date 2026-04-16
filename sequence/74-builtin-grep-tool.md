# Step 74 — Built-in Ripgrep-Backed `grep` Tool

_Add a native code search tool that respects .gitignore, auto-skips vendored directories, and is auto-approved. Eliminates the bash+rg approval overhead that dominated the 2026-04-15 e2e test._

---

## 1. What this step is about

In the 2026-04-15 e2e test, the brain used `bash` + `rg` (ripgrep) for every code search. Each bash invocation required approval because `bash` has `RequireApproval` policy. The brain made 10+ grep-style searches, each blocked by an approval prompt. Claude Code treats search as "essentially free" — it runs 10–30 ripgrep searches per task because its `Grep` tool is built-in and auto-approved.

MOA should have a first-class `grep` tool backed by the `grep` crate (Rust implementation of ripgrep's core) or by shelling out to `rg` with controlled flags. This tool:
- Is registered as a **read-only built-in**, auto-approved by default policy
- Respects `.gitignore` patterns via the `ignore` crate
- Uses the same `SKIPPED_SEARCH_DIRS` as `file_search`
- Returns results with file paths, line numbers, and match context
- Is far more token-efficient than raw `bash` + `rg` output (structured, truncated, deduped)

---

## 2. Files to read

- **`moa-hands/src/tools/file_search.rs`** — Existing glob-based file search. The new tool complements this (file_search finds files by name; grep finds content within files).
- **`moa-hands/src/tools/bash.rs`** — Current bash execution. The grep tool replaces the common `rg` use case.
- **`moa-hands/src/router/registration.rs`** — Tool registration. Register as a read-only built-in.
- **`moa-core/src/types/policy.rs`** — `read_tool_policy` for auto-approve.
- **`Cargo.toml` (moa-hands)** — Add `grep` crate dependency.

---

## 3. Goal

After this step:
1. A `grep` hand tool is available in the default loadout
2. It searches file contents using regex or literal patterns
3. It auto-skips `.git`, `.venv`, `node_modules`, and all `SKIPPED_SEARCH_DIRS` entries
4. It respects `.gitignore` if present in the workspace root
5. It is auto-approved (read-only policy), eliminating approval friction for searches
6. Results are structured: `{path}:{line}:{content}` with configurable context lines

---

## 4. Rules

- **Read-only policy.** Use `read_tool_policy(ToolInputShape::Pattern)` — same as `file_search`. Auto-approved in the default config.
- **Use the `grep` crate** (ripgrep's library form) rather than shelling out. This avoids spawning a subprocess, doesn't require `rg` to be installed, and is controllable from Rust.
- **Fallback: if the `grep` crate is too heavy**, use the `ignore` crate for directory walking + `regex` crate for matching. The `ignore` crate already respects `.gitignore` and custom ignore files.
- **Max results: 100 matches.** Truncate with a message. This prevents the 1000-match problem from the original e2e test.
- **Context lines: 0 by default**, configurable via `context_lines` parameter (max 5). This matches ripgrep's `-C` flag.
- **Binary files are skipped automatically.**
- **The tool schema should be simple:** `pattern` (required), `path` (optional, defaults to workspace root), `context_lines` (optional), `literal` (optional bool, default false — when true, treats pattern as literal string not regex).

---

## 5. Tasks

### 5a. Add dependencies to `moa-hands/Cargo.toml`

```toml
ignore = "0.4"   # .gitignore-aware directory walking (same as ripgrep)
regex = "1"      # already likely present
```

### 5b. Create `moa-hands/src/tools/grep.rs`

```rust
use std::path::Path;
use std::time::{Duration, Instant};

use ignore::WalkBuilder;
use moa_core::{Result, ToolContent, ToolOutput};
use regex::Regex;
use serde::Deserialize;

const MAX_MATCHES: usize = 100;
const MAX_CONTEXT_LINES: usize = 5;
const MAX_LINE_LENGTH: usize = 500;

pub async fn execute(sandbox_dir: &Path, input: &str, extra_skips: &[String]) -> Result<ToolOutput> {
    let params: GrepInput = serde_json::from_str(input)?;
    let started = Instant::now();

    let search_root = if let Some(ref subpath) = params.path {
        sandbox_dir.join(subpath)
    } else {
        sandbox_dir.to_path_buf()
    };

    let pattern = if params.literal.unwrap_or(false) {
        regex::escape(&params.pattern)
    } else {
        params.pattern.clone()
    };
    let regex = Regex::new(&pattern)
        .map_err(|e| moa_core::MoaError::ValidationError(format!("invalid regex: {e}")))?;

    let context_lines = params.context_lines.unwrap_or(0).min(MAX_CONTEXT_LINES);

    let mut matches = Vec::new();
    let mut files_searched = 0usize;

    let walker = WalkBuilder::new(&search_root)
        .hidden(true)        // skip hidden files
        .git_ignore(true)    // respect .gitignore
        .git_global(false)
        .git_exclude(true)
        .build();

    for entry in walker.flatten() {
        if matches.len() >= MAX_MATCHES {
            break;
        }
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();

        // Apply MOA's skip list on top of .gitignore
        let relative = path.strip_prefix(sandbox_dir).unwrap_or(path);
        if super::file_search::should_skip_search_path_static(relative, extra_skips) {
            continue;
        }

        // Skip binary files
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        files_searched += 1;

        let lines: Vec<&str> = content.lines().collect();
        for (line_idx, line) in lines.iter().enumerate() {
            if matches.len() >= MAX_MATCHES {
                break;
            }
            if regex.is_match(line) {
                let display_path = relative.display().to_string();
                let truncated_line = if line.len() > MAX_LINE_LENGTH {
                    format!("{}...", &line[..MAX_LINE_LENGTH])
                } else {
                    line.to_string()
                };

                if context_lines == 0 {
                    matches.push(format!("{}:{}:{}", display_path, line_idx + 1, truncated_line));
                } else {
                    let start = line_idx.saturating_sub(context_lines);
                    let end = (line_idx + context_lines + 1).min(lines.len());
                    let mut block = format!("{}:{}\n", display_path, line_idx + 1);
                    for ctx_idx in start..end {
                        let marker = if ctx_idx == line_idx { ">" } else { " " };
                        block.push_str(&format!("{} {:>5} | {}\n", marker, ctx_idx + 1, lines[ctx_idx]));
                    }
                    matches.push(block);
                }
            }
        }
    }

    let duration = started.elapsed();
    let hit_limit = matches.len() >= MAX_MATCHES;

    let mut summary = matches.join("\n");
    if hit_limit {
        summary.push_str(&format!(
            "\n\n[search truncated at {} matches; narrow the pattern or search a subdirectory]",
            MAX_MATCHES
        ));
    }
    summary.push_str(&format!("\n\n[{} files searched in {:?}]", files_searched, duration));

    Ok(ToolOutput {
        content: vec![ToolContent::Text { text: summary }],
        is_error: false,
        structured: Some(serde_json::json!({
            "match_count": matches.len(),
            "truncated": hit_limit,
            "files_searched": files_searched,
        })),
        duration,
    })
}

#[derive(Debug, Deserialize)]
struct GrepInput {
    pattern: String,
    path: Option<String>,
    context_lines: Option<usize>,
    literal: Option<bool>,
}
```

### 5c. Expose a static version of `should_skip_search_path`

In `file_search.rs`, make the path check callable without `&[String]` being async:

```rust
pub fn should_skip_search_path_static(path: &Path, extra_skips: &[String]) -> bool {
    // same logic as should_skip_search_path
}
```

### 5d. Register in `registration.rs`

```rust
registry.register_hand(
    "grep",
    "Search file contents using a regex pattern. Respects .gitignore and skips vendored directories. Returns matches with file paths and line numbers.",
    json!({
        "type": "object",
        "properties": {
            "pattern": { "type": "string", "description": "Regex pattern to search for. Use the literal flag for exact string matching." },
            "path": { "type": "string", "description": "Optional subdirectory to search within. Defaults to workspace root." },
            "context_lines": { "type": "integer", "minimum": 0, "maximum": 5, "description": "Lines of context around each match. Default: 0." },
            "literal": { "type": "boolean", "description": "When true, treat pattern as a literal string, not a regex. Default: false." }
        },
        "required": ["pattern"],
        "additionalProperties": false
    }),
    read_tool_policy(ToolInputShape::Pattern),
);
```

Add `grep` to the `default_loadout` vector, positioned after `file_search`.

### 5e. Route in `LocalHandProvider`

Add `"grep"` to the match in `execute()`.

### 5f. Add tests

Cover: basic regex match, literal mode, context lines, .gitignore respect, skip list respect, max matches truncation, subdirectory scoping, binary file skipping.

---

## 6. Deliverables

- [ ] `moa-hands/Cargo.toml` — `ignore` crate dependency
- [ ] `moa-hands/src/tools/grep.rs` (new) — Core implementation
- [ ] `moa-hands/src/tools/mod.rs` — Export module
- [ ] `moa-hands/src/tools/file_search.rs` — Expose static skip check
- [ ] `moa-hands/src/router/registration.rs` — Register tool
- [ ] `moa-hands/src/local.rs` — Execution routing
- [ ] Tests

---

## 7. Acceptance criteria

1. `grep` with pattern `class.*ViewSet` finds matches with file paths and line numbers.
2. `grep` does not return results from `.venv/`, `node_modules/`, or `.git/`.
3. `grep` does not return results from files listed in `.gitignore`.
4. `grep` is auto-approved (no approval prompt) when using default config.
5. Results are capped at 100 matches with a truncation message.
6. `context_lines: 2` shows 2 lines above and below each match.
7. `cargo test -p moa-hands` passes.

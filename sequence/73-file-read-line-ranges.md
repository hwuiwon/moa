# Step 73 — Partial File Reading with Line Ranges

_Add `start_line` and `end_line` parameters to `file_read` so the brain can inspect specific sections of large files without loading the entire contents into context._

---

## 1. What this step is about

The current `file_read` tool loads the entire file into context every time. For a 19,000-line file, that's ~76,000 tokens consumed for a single read — even when the brain only needs to see 50 lines around a function definition. Claude Code's `view` command supports `view_range: [start_line, end_line]` for exactly this reason. Sweep AI demonstrated that structural outlines with targeted line-range reads reduce context usage to **4.3–6.5%** of what full-file reads consume.

This step adds optional `start_line` / `end_line` parameters to `file_read`. The brain uses `file_search` or `bash`+`grep` to locate the relevant region, then reads only that range.

---

## 2. Files to read

- **`moa-hands/src/tools/file_read.rs`** — Current implementation. The `FileReadInput` struct needs new optional fields.
- **`moa-hands/src/tools/docker_file.rs`** — Docker-backed file read. Needs the same range support.
- **`moa-hands/src/router/registration.rs`** — Tool schema definition for `file_read`. Add the new parameters.
- **`moa-brain/src/pipeline/identity.rs`** — Prompt guidance about partial reads.
- **`moa-brain/src/pipeline/history.rs`** — `MAX_TOOL_RESULT_REPLAY_CHARS` and truncation. Partial reads reduce pressure here.

---

## 3. Goal

After this step:
1. `file_read` accepts optional `start_line` and `end_line` parameters
2. When provided, only the specified line range is returned, prefixed with line numbers
3. The output header shows `[lines 150-200 of 19000]` so the brain knows file context
4. When omitted, behavior is unchanged (full file, no line numbers)
5. The tool schema documents these parameters

---

## 4. Rules

- **Line numbers are 1-indexed.** `start_line: 1` is the first line of the file. Consistent with most editors and grep output.
- **`end_line` is inclusive.** `start_line: 10, end_line: 20` returns 11 lines (10 through 20).
- **Out-of-range values are clamped, not errored.** `end_line: 99999` on a 500-line file returns through line 500. `start_line: 0` is treated as 1.
- **Line numbers are prepended to each line** when a range is specified: `  150\t    def method(self):`. This helps the brain reference exact positions for subsequent `str_replace` calls. Use tab separator.
- **Full-file reads do NOT get line numbers.** This preserves backward compatibility and avoids inflating context for small files.
- **A file-length header is always included** when a range is specified: `[showing lines 150-200 of 19000 total in server/core/views.py]`
- **Maximum range cap: 500 lines.** If the range exceeds 500 lines, truncate and append `[output truncated to 500 lines; use a narrower range]`. This prevents the brain from requesting `start_line: 1, end_line: 19000`.

---

## 5. Tasks

### 5a. Update `FileReadInput` and `execute`

```rust
#[derive(Debug, Deserialize)]
struct FileReadInput {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

const MAX_READ_RANGE_LINES: usize = 500;

pub async fn execute(sandbox_dir: &Path, input: &str) -> Result<ToolOutput> {
    let params: FileReadInput = serde_json::from_str(input)?;
    let path = resolve_sandbox_path(sandbox_dir, &params.path)?;
    let content = fs::read_to_string(&path).await?;

    match (params.start_line, params.end_line) {
        (Some(start), end) => {
            let lines: Vec<&str> = content.lines().collect();
            let total_lines = lines.len();
            let start = start.max(1) - 1; // convert to 0-indexed
            let end = end.map(|e| e.min(total_lines)).unwrap_or(total_lines);
            let end = end.min(start + MAX_READ_RANGE_LINES);
            let selected = &lines[start..end.min(total_lines)];

            let mut output = format!(
                "[showing lines {}-{} of {} total in {}]\n",
                start + 1, end.min(total_lines), total_lines, params.path
            );
            for (i, line) in selected.iter().enumerate() {
                output.push_str(&format!("{:>6}\t{}\n", start + i + 1, line));
            }
            if end < total_lines && end - start >= MAX_READ_RANGE_LINES {
                output.push_str(&format!(
                    "\n[output truncated to {} lines; use a narrower range]\n",
                    MAX_READ_RANGE_LINES
                ));
            }
            Ok(ToolOutput::text(output, Duration::default()))
        }
        (None, None) => {
            // Full file read — unchanged behavior
            Ok(ToolOutput::text(content, Duration::default()))
        }
        (None, Some(end)) => {
            // end_line without start_line: read from beginning
            let lines: Vec<&str> = content.lines().collect();
            let total_lines = lines.len();
            let end = end.min(total_lines).min(MAX_READ_RANGE_LINES);
            let selected = &lines[..end];

            let mut output = format!(
                "[showing lines 1-{} of {} total in {}]\n",
                end, total_lines, params.path
            );
            for (i, line) in selected.iter().enumerate() {
                output.push_str(&format!("{:>6}\t{}\n", i + 1, line));
            }
            Ok(ToolOutput::text(output, Duration::default()))
        }
    }
}
```

### 5b. Update Docker execution path

Mirror the range logic in `execute_docker`, applying it to the content returned by `docker_file_read`.

### 5c. Update tool schema in `registration.rs`

```rust
registry.register_hand(
    "file_read",
    "Read a UTF-8 text file from the workspace root. Supports optional line range for large files.",
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Relative path within the workspace root." },
            "start_line": { "type": "integer", "minimum": 1, "description": "First line to read (1-indexed, inclusive). Omit to read from the beginning." },
            "end_line": { "type": "integer", "minimum": 1, "description": "Last line to read (1-indexed, inclusive). Omit to read to the end." }
        },
        "required": ["path"],
        "additionalProperties": false
    }),
    read_tool_policy(ToolInputShape::Path),
);
```

### 5d. Update identity prompt

Add to the coding guardrails section in `identity.rs`:

```text
- When reading large files (>200 lines), prefer partial reads with
  start_line/end_line to avoid flooding context. Use file_search or grep
  first to find the relevant line range, then read only that section.
```

### 5e. Add tests

```rust
#[tokio::test]
async fn file_read_with_line_range() {
    let dir = tempdir().unwrap();
    let content = (1..=100).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
    fs::write(dir.path().join("test.txt"), &content).await.unwrap();

    let output = execute(dir.path(), r#"{"path":"test.txt","start_line":10,"end_line":15}"#).await.unwrap();
    let text = output.to_text();
    assert!(text.contains("line 10"));
    assert!(text.contains("line 15"));
    assert!(!text.contains("line 9"));
    assert!(!text.contains("line 16"));
    assert!(text.contains("[showing lines 10-15 of 100 total"));
}

#[tokio::test]
async fn file_read_full_unchanged() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("small.txt"), "hello\nworld\n").await.unwrap();
    let output = execute(dir.path(), r#"{"path":"small.txt"}"#).await.unwrap();
    let text = output.to_text();
    assert_eq!(text.trim(), "hello\nworld");
    assert!(!text.contains("[showing lines"));  // no header for full reads
}

#[tokio::test]
async fn file_read_clamps_out_of_range() {
    let dir = tempdir().unwrap();
    let content = (1..=10).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
    fs::write(dir.path().join("test.txt"), &content).await.unwrap();
    let output = execute(dir.path(), r#"{"path":"test.txt","start_line":8,"end_line":999}"#).await.unwrap();
    let text = output.to_text();
    assert!(text.contains("line 8"));
    assert!(text.contains("line 10"));
    assert!(text.contains("[showing lines 8-10 of 10 total"));
}

#[tokio::test]
async fn file_read_truncates_large_range() {
    let dir = tempdir().unwrap();
    let content = (1..=1000).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
    fs::write(dir.path().join("big.txt"), &content).await.unwrap();
    let output = execute(dir.path(), r#"{"path":"big.txt","start_line":1,"end_line":1000}"#).await.unwrap();
    let text = output.to_text();
    assert!(text.contains("[output truncated to 500 lines"));
}
```

---

## 6. Deliverables

- [ ] `moa-hands/src/tools/file_read.rs` — Line range support with clamping and truncation
- [ ] `moa-hands/src/tools/docker_file.rs` — Docker range support
- [ ] `moa-hands/src/router/registration.rs` — Updated tool schema
- [ ] `moa-brain/src/pipeline/identity.rs` — Prompt guidance for partial reads
- [ ] Tests covering range reads, full reads, clamping, and truncation

---

## 7. Acceptance criteria

1. `file_read` with `start_line: 150, end_line: 200` on a 19,000-line file returns only lines 150–200 with line numbers.
2. `file_read` without range parameters returns the full file without line numbers (backward compatible).
3. Out-of-range values are clamped without error.
4. Ranges exceeding 500 lines are truncated with a message.
5. The identity prompt mentions partial reads for large files.
6. `cargo test -p moa-hands` passes.

# Step 69 — Workspace Instruction File Discovery (AGENTS.md)

_Auto-discover and load project-level instruction files from the workspace root. Follows the emerging AGENTS.md convention adopted by Codex CLI, Claude Code, Cursor, and 60,000+ repositories._

---

## 1. What this step is about

The `InstructionProcessor` (pipeline stage 2) currently only reads workspace/user instructions from `MoaConfig` — global strings set in `~/.moa/config.toml`. There is no mechanism to discover project-local instruction files from the target workspace root. When MOA operates on `~/github/applied`, it has zero project-specific guidance about the Django codebase, its test commands, or conventions.

The industry has converged on `AGENTS.md` as the cross-tool standard (Linux Foundation stewardship, supported by Codex CLI, Claude Code, Cursor, Cline, Windsurf, GitHub Copilot, and others). MOA should discover and load it.

---

## 2. Files to read

- **`moa-brain/src/pipeline/instructions.rs`** — Current `InstructionProcessor`. This is where workspace file content will be injected.
- **`moa-brain/src/pipeline/mod.rs`** — Pipeline construction. The discovered `AGENTS.md` content needs to be passed in from the session start path.
- **`moa-orchestrator/src/local.rs`** — Where the workspace root is resolved and sessions are started. `AGENTS.md` should be reread for every new session.
- **`moa-runtime/src/local.rs`** — `LocalChatRuntime` construction. Another path where workspace root is known.
- **`moa-core/src/config.rs`** — `GeneralConfig.workspace_instructions`. The discovered file content supplements (not replaces) this config field.
- **`moa-memory/src/bootstrap.rs`** — Ensure `AGENTS.md` is not copied into memory. It remains the prompt-time source of truth.

---

## 3. Goal

After this step:
1. When a new session starts, MOA reads `AGENTS.md` from the workspace root
2. The file is loaded fresh for every new session start (up to 32 KiB, matching Codex CLI's default) and injected into the context via `InstructionProcessor`
3. Config-based `workspace_instructions` and the discovered `AGENTS.md` content are combined (config first, file second)
4. If no instruction file exists, behavior is unchanged
5. `AGENTS.md` is not copied into workspace memory; it remains the live source of truth for project instructions

---

## 4. Rules

- **Supported filename: `AGENTS.md` only.** Do NOT add fallback names such as `CLAUDE.md` or `.moa/instructions.md`.
- **Size cap: 32 KiB** (matching Codex CLI's `project_doc_max_bytes`). Files larger than this are truncated with a warning log. Research shows files over ~150 lines degrade adherence.
- **Read the file again for every new session.** `AGENTS.md` may change while the runtime is alive, so do not cache its content across sessions.
- **Pipeline stage stays pure.** The pipeline stage itself does no filesystem I/O; discovery happens in the session start path once the workspace root is known.
- **No directory traversal upward.** Unlike Claude Code (which walks to `/`), MOA only checks the workspace root. This is simpler and avoids loading unrelated ancestor instructions.
- **File content is injected as a `<workspace_instructions>` block**, same tag as config-based instructions. If both exist, they are concatenated with a separator.
- **`AGENTS.md` is prompt-time state, not memory state.** Do not copy it into workspace memory or treat memory bootstrap as the source of truth.

---

## 5. Tasks

### 5a. Add instruction file discovery function

Create `moa-core/src/workspace.rs` (or add to existing workspace utilities):

```rust
use std::path::Path;

const INSTRUCTION_FILE_NAME: &str = "AGENTS.md";

const MAX_INSTRUCTION_FILE_BYTES: usize = 32_768; // 32 KiB

/// Discovers and loads `AGENTS.md` from the given workspace root.
/// Returns `None` if the file does not exist.
pub fn discover_workspace_instructions(workspace_root: &Path) -> Option<String> {
    let path = workspace_root.join(INSTRUCTION_FILE_NAME);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(_) => return None,
    };

    if content.len() > MAX_INSTRUCTION_FILE_BYTES {
        tracing::warn!(
            path = %path.display(),
            size = content.len(),
            max = MAX_INSTRUCTION_FILE_BYTES,
            "workspace instruction file exceeds size limit, truncating"
        );
        let truncated = &content[..MAX_INSTRUCTION_FILE_BYTES];
        let end = truncated.rfind('\n').unwrap_or(MAX_INSTRUCTION_FILE_BYTES);
        return Some(content[..end].to_string());
    }

    tracing::info!(
        path = %path.display(),
        size = content.len(),
        "loaded workspace instruction file"
    );
    Some(content)
}
```

### 5b. Update `InstructionProcessor` to accept discovered instructions

```rust
impl InstructionProcessor {
    pub fn new(
        workspace_instructions: Option<String>,
        user_instructions: Option<String>,
        discovered_instructions: Option<String>,
    ) -> Self {
        // Combine config-based and discovered workspace instructions
        let combined_workspace = match (workspace_instructions, discovered_instructions) {
            (Some(config), Some(discovered)) => Some(format!(
                "{config}\n\n---\n\n{discovered}"
            )),
            (Some(config), None) => Some(config),
            (None, Some(discovered)) => Some(discovered),
            (None, None) => None,
        };

        Self {
            workspace_instructions: combined_workspace,
            user_instructions,
        }
    }
}
```

### 5c. Load instructions on every new session start

In the `LocalOrchestrator` session start path, after resolving the workspace root:

```rust
let discovered_instructions =
    moa_core::workspace::discover_workspace_instructions(&workspace_root);

// Pass to pipeline construction
let instruction_processor = InstructionProcessor::new(
    config.general.workspace_instructions.clone(),
    config.general.user_instructions.clone(),
    discovered_instructions,
);
```

### 5d. Keep `AGENTS.md` out of memory bootstrap

`AGENTS.md` should not be copied into workspace memory. If the bootstrap flow currently ingests `AGENTS.md`, remove that behavior so prompt injection is the only source of truth for project-local agent instructions.

### 5e. Add tests

```rust
#[test]
fn discovers_agents_md() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("AGENTS.md"), "Use pytest for testing.").unwrap();
    let result = discover_workspace_instructions(dir.path());
    assert_eq!(result.as_deref(), Some("Use pytest for testing."));
}

#[test]
fn ignores_non_agents_instruction_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("CLAUDE.md"), "Conventions here.").unwrap();
    std::fs::create_dir(dir.path().join(".moa")).unwrap();
    std::fs::write(dir.path().join(".moa/instructions.md"), "MOA specific.").unwrap();
    let result = discover_workspace_instructions(dir.path());
    assert!(result.is_none());
}

#[test]
fn returns_none_when_no_file_exists() {
    let dir = tempfile::tempdir().unwrap();
    assert!(discover_workspace_instructions(dir.path()).is_none());
}

#[test]
fn truncates_oversized_files() {
    let dir = tempfile::tempdir().unwrap();
    let large = "x\n".repeat(20_000); // ~40KB
    std::fs::write(dir.path().join("AGENTS.md"), &large).unwrap();
    let result = discover_workspace_instructions(dir.path()).unwrap();
    assert!(result.len() <= 32_768);
    assert!(result.ends_with('\n') || result.ends_with('x'));
}

#[tokio::test]
async fn workspace_instruction_file_is_reloaded_for_each_new_session() {
    // Start one session, update AGENTS.md, start another session,
    // and assert the second prompt contains the updated content.
}
```

---

## 6. Deliverables

- [ ] `moa-core/src/workspace.rs` (new or extended) — `discover_workspace_instructions` function
- [ ] `moa-brain/src/pipeline/instructions.rs` — Accept `discovered_instructions` parameter
- [ ] `moa-orchestrator/src/local.rs` — Load `AGENTS.md` on every new session start
- [ ] `moa-memory/src/bootstrap.rs` — Ensure `AGENTS.md` is not copied into memory
- [ ] Tests covering `AGENTS.md` discovery, non-fallback behavior, truncation, no-file case, and reread-per-session behavior

---

## 7. Acceptance criteria

1. An `AGENTS.md` in the workspace root is loaded into the brain's context at session start.
2. If `AGENTS.md` is edited between sessions, the next new session sees the updated content.
3. If no instruction file exists, behavior is identical to before this step.
4. Files over 32 KiB are truncated at a line boundary with a warning log.
5. Config-based `workspace_instructions` and discovered file content are both present when both are set.
6. `AGENTS.md` is not copied into workspace memory; prompt injection remains the source of truth.
7. `cargo test -p moa-core -p moa-brain` passes.

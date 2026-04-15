# Step 70 — Coding Guardrails in the Identity Prompt

_Extend the brain's default system prompt with concrete guidance for code editing tasks. Addresses the search noise, unsafe edits, and missing verification observed in the 2026-04-15 e2e test._

---

## 1. What this step is about

The `DEFAULT_IDENTITY_PROMPT` in `identity.rs` is five generic sentences. When the brain tackled a code refactoring task, it had no guidance about:
- Skipping vendored directories when searching
- Preferring `file_write` over bash heredocs/sed/python-c for file edits
- Running the project's test suite after changes (not just a linter)
- Keeping diffs scoped to the requested area
- Stopping after repeated failures instead of thrashing

These are not task-type-specific preferences — they apply to virtually every code editing task. Adding them to the base identity prompt is the right default. Projects that need different behavior can override via `AGENTS.md` (Step 69).

---

## 2. Files to read

- **`moa-brain/src/pipeline/identity.rs`** — `DEFAULT_IDENTITY_PROMPT` constant. The only file to modify.
- **`moa-hands/src/tools/file_search.rs`** — The skip list from Step 68, for reference in the prompt wording.

---

## 3. Goal

After this step:
1. The identity prompt includes concrete coding task guidance
2. The prompt remains general-purpose — it doesn't assume every task is coding
3. The added text is under 300 tokens to preserve cache efficiency

---

## 4. Rules

- **Keep the total identity prompt under 600 tokens.** The current prompt is ~200 tokens. The addition should be under 300 tokens. This is a stable-prefix element — every token counts.
- **Do NOT add a separate "coding profile" system.** That's a future step. This is about sensible defaults.
- **The guidance must be actionable, not philosophical.** "Prefer file_write for edits" is actionable. "Write clean code" is not.
- **The guidance must not conflict with `AGENTS.md` content.** Frame it as defaults that project instructions can override.

---

## 5. Tasks

### 5a. Extend `DEFAULT_IDENTITY_PROMPT`

Append the following block to the existing identity prompt, after the existing paragraphs:

```text
When working in code repositories:
- Skip vendored and generated directories (.venv, node_modules, __pycache__,
  target, vendor, .git, etc.) when searching. The file_search tool excludes
  these automatically. When using bash with grep or ripgrep, add exclusion
  flags yourself.
- Prefer the file_write tool for targeted code edits. Avoid bash-based text
  manipulation (sed, python -c, heredocs) for modifying source files — these
  are fragile and hard to verify.
- After making code changes, always run the project's test suite or relevant
  tests to verify correctness. A linter or formatter pass alone is not
  sufficient verification. Look for test commands in AGENTS.md, Makefile,
  package.json, or pyproject.toml.
- Keep changes scoped to what was requested. Do not run whole-file formatters
  that rewrite unrelated code.
- If you encounter errors in your own edits, fix them immediately. If you
  cannot converge after 3 attempts at the same fix, stop and report what
  went wrong instead of continuing to thrash.
```

### 5b. Verify token count

Estimate token count of the final prompt. It should be under 600 tokens total. The coding block above is approximately 200 tokens. Combined with the existing ~200 token prompt, the total should be ~400 tokens — well within budget. Adjust wording if the real count exceeds 600.

### 5c. Update the test

Update the existing `identity_processor_appends_system_prompt` test to verify the new content is present:

```rust
#[tokio::test]
async fn identity_prompt_includes_coding_guardrails() {
    let session = SessionMeta {
        id: SessionId::new(),
        workspace_id: WorkspaceId::new("workspace"),
        user_id: UserId::new("user"),
        platform: Platform::Tui,
        model: "claude-sonnet-4-6".to_string(),
        ..SessionMeta::default()
    };
    let capabilities = ModelCapabilities {
        model_id: "claude-sonnet-4-6".to_string(),
        context_window: 200_000,
        max_output: 8_192,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: true,
        cache_ttl: None,
        tool_call_format: ToolCallFormat::Anthropic,
        pricing: TokenPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cached_input_per_mtok: Some(0.3),
        },
        native_tools: Vec::new(),
    };
    let mut ctx = WorkingContext::new(&session, capabilities);

    IdentityProcessor::default().process(&mut ctx).await.unwrap();

    let content = &ctx.messages[0].content;
    assert!(content.contains("file_write tool for targeted code edits"));
    assert!(content.contains("test suite"));
    assert!(content.contains("3 attempts"));
    assert!(content.contains(".venv"));
}
```

---

## 6. Deliverables

- [ ] `moa-brain/src/pipeline/identity.rs` — Extended `DEFAULT_IDENTITY_PROMPT`
- [ ] Updated/new test verifying coding guardrails are present

---

## 7. Acceptance criteria

1. The identity prompt contains actionable coding guidance about search exclusions, file_write preference, test verification, scoped changes, and thrashing prevention.
2. Total prompt is under 600 tokens.
3. The prompt remains applicable to non-coding tasks (it says "when working in code repositories", not "always").
4. `cargo test -p moa-brain` passes.

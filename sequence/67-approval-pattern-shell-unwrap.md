# Step 67 — Fix Approval Pattern Derivation: Shell Wrapper Unwrapping

_Parse through `zsh -lc` / `bash -c` wrappers before deriving "Always Allow" patterns. Prevents the `zsh *` approval bomb discovered in the 2026-04-15 e2e test._

---

## 1. What this step is about

When a user selects "Always Allow" for a bash tool call, MOA derives a glob pattern from the command and persists it as an `ApprovalRule`. The current implementation in `moa-hands/src/router/normalization.rs:approval_pattern_for` takes the first shell token and appends ` *`. Because the local bash tool wraps all commands in `zsh -lc "..."`, the derived pattern is always `zsh *` — which matches every subsequent bash command for that workspace. This is a security-critical bug: a single "Always Allow" on any command grants blanket approval.

Claude Code, Codex CLI, and other production agents have all faced variants of this problem. No production framework reliably parses through shell wrappers (see CVE-2026-29607 in OpenClaw for the same class of bug). MOA must do better: unwrap the shell invocation, extract the inner command, and derive the pattern from that.

---

## 2. Files to read

- **`moa-hands/src/router/normalization.rs`** — `approval_pattern_for()` is the function to fix. Also read `normalized_input_for()` and `summary_for()` which share the same input pipeline.
- **`moa-hands/src/tools/bash.rs`** — How bash commands are executed. Understand the `zsh -lc` wrapping that happens at the tool level.
- **`moa-security/src/policies.rs`** — `parse_and_match_bash()` and `split_shell_chain()`. These are the matching functions that consume the patterns. The fix must produce patterns compatible with these matchers.
- **`moa-core/src/types/policy.rs`** — `ApprovalRule`, `ToolInputShape`, `PolicyAction` types.
- **`moa-hands/src/router/policy.rs`** — Where `approval_pattern_for` is called during the `prepare_invocation` flow.

---

## 3. Goal

After this step:
1. `approval_pattern_for` unwraps `zsh -lc "..."`, `bash -c "..."`, `sh -c "..."` wrappers before extracting the pattern
2. The derived pattern is based on the **inner command's first token**, not the shell wrapper
3. Chained inner commands (`cmd1 && cmd2`) produce a pattern from the first sub-command only
4. Existing `parse_and_match_bash` matching still works correctly with the new patterns
5. The `zsh *` pattern from the e2e test is impossible to produce through this code path

---

## 4. Rules

- **Unwrap exactly one layer.** If the inner command is itself a `bash -c`, do NOT recursively unwrap. One layer is the common case; deeper nesting is adversarial and should fall back to the full normalized input as the pattern.
- **Recognized shell wrappers:** `zsh -lc`, `zsh -c`, `bash -lc`, `bash -c`, `sh -c`. The `-l` (login shell) flag is optional. The wrapper must have exactly these forms — don't try to parse arbitrary shell flag combinations.
- **Pattern derivation from inner command:** Use the first token of the inner command for the glob (e.g., `rg -n` → `rg *`, `npm test` → `npm *`). Single-token commands produce an exact match (e.g., `pwd` → `pwd`). Multi-token commands produce `first_token *`.
- **If the inner command is itself chained** (`&&`, `||`, `;`, `|`), derive the pattern from the **first sub-command only**. This is conservative: approving `cd server && rg pattern .` stores a pattern for `cd *`, not `rg *`. The user can always approve the next command separately.
- **If unwrapping fails** (malformed quoting, no inner command found), fall back to storing the full normalized input as the pattern. Never silently produce `zsh *` or `bash *`.
- **Do NOT change `parse_and_match_bash`.** The matching side is correct. Only the pattern derivation side needs fixing.

---

## 5. Tasks

### 5a. Add `unwrap_shell_wrapper` function to `normalization.rs`

```rust
/// Recognized login/interactive shell wrapper prefixes.
const SHELL_WRAPPERS: &[(&str, &[&str])] = &[
    ("zsh", &["-lc", "-c"]),
    ("bash", &["-lc", "-c"]),
    ("sh", &["-c"]),
];

/// Attempts to extract the inner command from a shell wrapper invocation.
/// Returns `None` if the input is not a recognized wrapper form.
pub(super) fn unwrap_shell_wrapper(normalized_input: &str) -> Option<String> {
    let tokens = shell_words::split(normalized_input).ok()?;
    if tokens.len() < 3 {
        return None;
    }

    for (shell, flags) in SHELL_WRAPPERS {
        if tokens[0] != *shell {
            continue;
        }
        // Check for -lc (two separate tokens) or combined -lc/-c (one token)
        let inner_start = if tokens.len() >= 4
            && tokens[1] == "-l"
            && tokens[2] == "-c" {
            3
        } else if flags.iter().any(|flag| tokens[1] == *flag) {
            2
        } else {
            continue;
        };

        // The inner command is the next token (the quoted string)
        if inner_start < tokens.len() {
            return Some(tokens[inner_start].clone());
        }
    }

    None
}
```

Note: `zsh -lc "cmd"` may tokenize as `["zsh", "-lc", "cmd"]` (combined flag) or `["zsh", "-l", "-c", "cmd"]` (separate flags). Handle both forms.

### 5b. Update `approval_pattern_for` to use unwrapping

```rust
pub(super) fn approval_pattern_for(input_shape: ToolInputShape, normalized_input: &str) -> String {
    if matches!(input_shape, ToolInputShape::Command) {
        // Try to unwrap shell wrappers first
        let effective_command = unwrap_shell_wrapper(normalized_input)
            .unwrap_or_else(|| normalized_input.to_string());

        // If chained, use only the first sub-command
        let sub_commands = split_shell_chain(&effective_command);
        let target = sub_commands.first()
            .map(|s| s.as_str())
            .unwrap_or(&effective_command);

        let tokens = shell_words::split(target).unwrap_or_default();
        if let Some(command) = tokens.first() {
            // Guard: never produce a pattern for a bare shell name
            if matches!(command.as_str(), "zsh" | "bash" | "sh" | "dash" | "fish") {
                // Unwrapping failed to extract inner command; store full input
                return normalized_input.to_string();
            }
            return if tokens.len() == 1 {
                command.clone()
            } else {
                format!("{command} *")
            };
        }
    }

    normalized_input.to_string()
}
```

The explicit guard against bare shell names (`zsh`, `bash`, `sh`) is a safety net: if unwrapping somehow still produces a shell name as the first token, we refuse to create a wildcard pattern for it.

### 5c. Import `split_shell_chain` in `normalization.rs`

`split_shell_chain` currently lives in `moa-security/src/policies.rs`. Either:
- Re-export it from `moa_security` and import it in `moa-hands`, OR
- Move it to `moa-core` as a shared utility (preferred, since both crates need it)

If moving to `moa-core`, place it in a new `moa-core/src/shell.rs` module.

### 5d. Add comprehensive tests

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use moa_core::ToolInputShape;

    #[test]
    fn unwrap_zsh_lc_wrapper() {
        let input = r#"zsh -lc "cd server && rg -n 'class CallViewSet' .""#;
        let inner = unwrap_shell_wrapper(input).expect("should unwrap");
        assert_eq!(inner, "cd server && rg -n 'class CallViewSet' .");
    }

    #[test]
    fn unwrap_bash_c_wrapper() {
        let input = r#"bash -c "npm test""#;
        let inner = unwrap_shell_wrapper(input).expect("should unwrap");
        assert_eq!(inner, "npm test");
    }

    #[test]
    fn no_unwrap_for_plain_command() {
        assert!(unwrap_shell_wrapper("npm test").is_none());
        assert!(unwrap_shell_wrapper("rg -n pattern .").is_none());
    }

    #[test]
    fn approval_pattern_unwraps_zsh_wrapper() {
        let pattern = approval_pattern_for(
            ToolInputShape::Command,
            r#"zsh -lc "cd server && rg -n 'class' .""#,
        );
        assert_eq!(pattern, "cd *");
        assert_ne!(pattern, "zsh *"); // The critical assertion
    }

    #[test]
    fn approval_pattern_simple_command() {
        let pattern = approval_pattern_for(
            ToolInputShape::Command,
            "npm test",
        );
        assert_eq!(pattern, "npm *");
    }

    #[test]
    fn approval_pattern_single_token() {
        let pattern = approval_pattern_for(
            ToolInputShape::Command,
            "pwd",
        );
        assert_eq!(pattern, "pwd");
    }

    #[test]
    fn approval_pattern_nested_shell_not_recursed() {
        // bash -c "bash -c 'rm -rf /'" should NOT produce rm *
        let pattern = approval_pattern_for(
            ToolInputShape::Command,
            r#"bash -c "bash -c 'rm -rf /'""#,
        );
        // Inner command is "bash -c 'rm -rf /'" — first token is bash,
        // which triggers the shell-name guard → stores full input
        assert!(!pattern.starts_with("rm"));
    }

    #[test]
    fn approval_pattern_chained_inner_uses_first_subcommand() {
        let pattern = approval_pattern_for(
            ToolInputShape::Command,
            r#"zsh -lc "npm install && npm test""#,
        );
        assert_eq!(pattern, "npm *");
    }
}
```

### 5e. Migrate existing `zsh *` rules

Add a one-time migration that scans `approval_rules` for patterns matching bare shell names (`zsh *`, `bash *`, `sh *`) and deletes them with a warning log:

```rust
pub async fn cleanup_overly_broad_shell_rules(
    store: &dyn ApprovalRuleStore,
    workspace_id: &WorkspaceId,
) -> Result<usize> {
    let rules = store.list_approval_rules(workspace_id).await?;
    let mut cleaned = 0;
    for rule in rules {
        if rule.tool == "bash"
            && matches!(rule.pattern.as_str(), "zsh *" | "bash *" | "sh *" | "dash *")
        {
            tracing::warn!(
                workspace_id = %workspace_id,
                pattern = %rule.pattern,
                "deleting overly broad shell approval rule"
            );
            store.delete_approval_rule(workspace_id, &rule.tool, &rule.pattern).await?;
            cleaned += 1;
        }
    }
    Ok(cleaned)
}
```

Call this during workspace initialization in `LocalOrchestrator` or `LocalChatRuntime`.

---

## 6. Deliverables

- [ ] `moa-core/src/shell.rs` (new) — `split_shell_chain` moved here from `moa-security`
- [ ] `moa-hands/src/router/normalization.rs` — `unwrap_shell_wrapper` added, `approval_pattern_for` rewritten
- [ ] `moa-security/src/policies.rs` — `split_shell_chain` re-exported from `moa-core` instead of defined here
- [ ] `moa-security/src/policies.rs` — `cleanup_overly_broad_shell_rules` added
- [ ] Tests in `normalization.rs` covering all wrapper forms and edge cases
- [ ] Existing tests in `moa-security` still pass (no behavior change in matching)

---

## 7. Acceptance criteria

1. `approval_pattern_for(Command, "zsh -lc \"rg -n pattern .\"")` returns `rg *`, not `zsh *`.
2. `approval_pattern_for(Command, "npm test")` still returns `npm *` (non-wrapped commands unchanged).
3. Nested shell wrappers (`bash -c "bash -c 'rm -rf /'"`) do NOT produce `rm *` — the shell-name guard prevents it.
4. The existing `parse_and_match_bash` tests in `moa-security` pass without changes.
5. No existing approval rules with pattern `zsh *` survive workspace initialization.
6. `cargo test -p moa-hands -p moa-security` passes.

# Step 07: Approval Flow + Permission Policies

## What this step is about
Adding the approval system between tool calls. The brain checks if a tool call needs approval, pauses for a decision, and respects persistent "Always Allow" rules.

## Files to read
- `docs/03-communication-layer.md` — Three-tier buttons, rule storage, command parsing
- `docs/08-security.md` — Tool permission policies, command-level matching

## Goal
When the brain wants to execute a tool, it checks the permission policy. If approval is needed, it emits an `ApprovalRequested` event and waits for an `ApprovalDecided` signal. "Always Allow" rules persist per-workspace.

## Rules
- Default policy: read tools auto-approved, write/exec tools require approval
- "Always Allow" stores a rule in the session database (`approval_rules` table)
- Rules match at the parsed command level (not raw string) — see `docs/08-security.md`
- Risk levels: Low (read-only), Medium (file writes), High (shell, network, destructive)
- The brain must NOT execute the tool until approval is received

## Tasks
1. **`moa-security/src/policies.rs`**: `ToolPolicies` struct, `check()` method, rule matching
2. **`moa-security/src/lib.rs`**: Expose policies
3. **Update `moa-session/src/turso.rs`**: CRUD for `approval_rules` table
4. **Update `moa-brain/src/harness.rs`**: Before executing a tool, check policy. If approval needed, emit `ApprovalRequested` and return `TurnResult::NeedsApproval`. On `ApprovalDecided(AlwaysAllow)`, store the rule.
5. **Update `moa-hands/src/router.rs`**: `ToolRouter.execute()` checks policies before dispatching

## Deliverables
```
moa-security/src/
├── lib.rs
└── policies.rs      # ToolPolicies + rule matching
```
Plus updates to `moa-brain/harness.rs` and `moa-session/turso.rs`.

## Acceptance criteria
1. Read tools (file_read, file_search, memory_search) auto-approved
2. Write tools (file_write) pause for approval
3. Bash tool pauses for approval
4. "Always Allow" stores a persistent rule
5. Previously approved tool+pattern combo skips approval on next call
6. Shell command parsing prevents wrapper bypass (`npm test && rm -rf /` is not matched by `npm test*` rule)

## Tests
- Unit test: Policy check returns `Allow` for file_read, `RequireApproval` for bash
- Unit test: Rule matching with glob patterns
- Unit test: Shell command parsing detects chained commands
- Integration test: Brain turn with approval → inject ApprovalDecided → tool executes → verify ToolResult event
- Integration test: Store AlwaysAllow rule → next call skips approval

```bash
cargo test -p moa-security
cargo test -p moa-brain
```

---


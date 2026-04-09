# Step 09: TUI Approval Widgets + Diff View

## What this step is about
Rich approval widgets with diff preview, risk-level coloring, and the full-screen diff viewer.

## Files to read
- `docs/03-communication-layer.md` — Approval prompt layout, diff view shortcuts, risk coloring

## Goal
When the agent wants to write a file, the user sees a bordered approval card with the diff inline. They can press `d` to expand to a full-screen diff viewer with side-by-side/unified toggle.

## Tasks
1. **`moa-tui/src/views/diff.rs`**: Full-screen diff viewer (side-by-side at 120+ cols, unified otherwise). Uses `similar` crate for diff algorithm, `syntect` for syntax highlighting. Shortcuts: `t` toggle mode, `n/N` next/prev file, `j/k` next/prev hunk, `a` accept, `r` reject, `Esc` close.
2. **`moa-tui/src/widgets/approval.rs`**: Approval card with risk-level coloring (green/yellow/red border), tool name, parameter summary, compact diff preview for file writes. Shortcuts: `y` allow, `n` deny, `a` always, `d` open diff, `e` edit params.
3. **Update `moa-tui/src/views/chat.rs`**: Render approval widgets inline in the message stream. When approval is focused, route keyboard to approval shortcuts.

## Deliverables
`moa-tui/src/views/diff.rs`, `moa-tui/src/widgets/approval.rs`, updated `chat.rs`

## Acceptance criteria
1. Approval cards render with correct risk coloring
2. `y/n/a` shortcuts work and emit the correct `ApprovalDecided` signal
3. `d` opens full-screen diff with syntax highlighting
4. Side-by-side / unified toggle works
5. After decision, approval card updates to show result ("✅ Allowed")

## Tests
- Unit test: Diff view layout calculates correctly for various terminal widths
- Unit test: Approval card renders with correct border color for each risk level
- Manual test: Agent writes a file → approval shows diff → approve → file written

---


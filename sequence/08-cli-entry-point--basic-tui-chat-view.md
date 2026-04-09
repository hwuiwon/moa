# Step 08: CLI Entry Point + Basic TUI Chat View

## What this step is about
The `moa` binary that launches an interactive TUI, and the `moa exec` subcommand for one-shot non-interactive use. The TUI at this stage is a basic chat view: prompt input at the bottom, scrolling message history above, streaming LLM output.

## Files to read
- `docs/03-communication-layer.md` — CLI subcommands, TUI layout (5 zones), prompt features, slash commands
- `docs/10-technology-stack.md` — Crates: `ratatui`, `crossterm`, `tui-textarea`, `clap`

## Goal
Run `moa` → see a TUI → type a message → see streaming response → see tool calls rendered inline → approve/deny tools with keyboard shortcuts. Run `moa exec "question"` → get answer on stdout.

## Rules
- `moa-cli` handles argument parsing with `clap` and dispatches to TUI or exec mode
- `moa-tui` owns the ratatui rendering loop
- TUI runs at 30 FPS with crossterm event polling
- Prompt input uses `tui-textarea` for multiline editing
- `Enter` submits, `Shift+Enter` inserts newline
- Streaming output renders character by character
- Tool calls render as inline bordered cards with tool name + status
- `Ctrl+C` cancels current generation
- `Escape` also cancels
- Slash commands: `/help`, `/model`, `/quit`, `/clear` at minimum

## Tasks
1. **`moa-cli/src/main.rs`**: Clap-based argument parsing. Subcommands: (default → TUI), `exec`, `version`, `doctor`
2. **`moa-tui/src/main.rs`**: Entry point that initializes terminal, creates App, runs render loop
3. **`moa-tui/src/app.rs`**: App state machine (Idle, Composing, Running, WaitingApproval)
4. **`moa-tui/src/views/chat.rs`**: Chat view — message list + streaming output
5. **`moa-tui/src/widgets/prompt.rs`**: Prompt input widget wrapping `tui-textarea`
6. **`moa-tui/src/widgets/tool_card.rs`**: Inline tool call card (bordered, with status icon)
7. **`moa-tui/src/keybindings.rs`**: Key event dispatch
8. **`moa-cli/src/exec.rs`**: Non-interactive mode — create session, submit prompt, stream events to stderr, print final response to stdout
9. **Wire everything together**: CLI creates the `LocalOrchestrator` (from Step 10) or a simplified single-session runner, starts TUI

**Important**: At this stage, use a simplified single-session runner (not the full LocalOrchestrator from Step 10). The TUI creates one brain directly, sends messages, and renders responses. Multi-session comes in Steps 10-11.

## Deliverables
```
moa-cli/src/
├── main.rs          # clap argument parsing + dispatch
└── exec.rs          # non-interactive mode

moa-tui/src/
├── main.rs          # terminal init + render loop
├── app.rs           # App state machine
├── views/
│   └── chat.rs      # chat message list
├── widgets/
│   ├── prompt.rs    # text input
│   └── tool_card.rs # inline tool rendering
└── keybindings.rs   # key dispatch
```

## Acceptance criteria
1. `moa` launches a TUI with a prompt at the bottom
2. Type a message + Enter → streamed response appears above
3. Tool calls show as inline cards with name and status
4. Approval prompts show inline with `y/n/a` shortcuts
5. `Ctrl+C` cancels active generation
6. `/quit` exits cleanly
7. `/clear` clears the chat display
8. `moa exec "What is 2+2?"` prints "4" (or equivalent) to stdout and exits
9. `moa version` prints version info
10. Terminal restores cleanly on exit (no garbled screen)

## Tests
- Unit test: App state transitions (Idle → Composing → Running → Idle)
- Unit test: Key event dispatch maps correctly
- Unit test: Exec mode formats output correctly
- Manual test: Run `moa`, chat, approve a tool, verify response
- Manual test: Run `moa exec "list files in current directory"`, verify output
- Automated smoke test: Start TUI, send synthetic key events, verify no panic

```bash
cargo build -p moa-cli
cargo build -p moa-tui
# Manual testing:
./target/debug/moa
./target/debug/moa exec "What is 2+2?"
./target/debug/moa version
```

## Notes
- The TUI at this stage does NOT have: tabs, sidebar, session picker, memory browser, settings panel. Those come in later steps.
- The header zone shows: "MOA" + model name + token count. No cost tracking yet.
- The footer shows: mode + basic shortcuts (`Enter: send | Ctrl+C: cancel | /help`)
- For `moa exec`, detect if stdout is a TTY. If not, suppress ANSI codes.

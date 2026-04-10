# Step 21: TUI Polish

## What this step is about
Adding the sidebar, memory browser, settings panel, workspace switcher, and all remaining keyboard shortcuts.

## Files to read
- `docs/03-communication-layer.md` — Full TUI layout (5 zones), all views, all shortcuts

## Goal
The TUI is feature-complete: sidebar with session info and tools, memory browser with two-pane wiki layout, settings panel, workspace switcher, command palette, and all documented keyboard shortcuts.

## Tasks
1. **`moa-tui/src/widgets/sidebar.rs`**: Sidebar (auto-show at 120+ cols, toggle with `Ctrl+X, B`). Session info, workspace tools, recent memory entries.
2. **`moa-tui/src/views/memory.rs`**: Memory browser — tree on left, rendered markdown on right. FTS search, `[[wikilink]]` navigation, back/forward.
3. **`moa-tui/src/views/settings.rs`**: Settings panel — categories on left, form widgets on right. `rat-widget` for inputs.
4. **Header zone**: Workspace name, model, token usage, cumulative cost.
5. **Command palette**: `Ctrl+P` overlay with fuzzy search via `nucleo`. Lists all actions with keybindings.
6. **Full keyboard shortcut implementation**: All shortcuts from docs (leader-key `Ctrl+X` then action key).
7. **`/slash` command completion**: Tab-complete dropdown with all registered commands.
8. **`@file` completion**: Frecency-ranked file path autocomplete.

## Deliverables
`moa-tui/src/widgets/sidebar.rs`, `moa-tui/src/views/memory.rs`, `moa-tui/src/views/settings.rs`, command palette, updated keybindings.

## Acceptance criteria
1. Sidebar shows and hides correctly based on terminal width
2. Memory browser navigates wiki pages with wikilinks
3. Settings panel edits config and hot-reloads
4. Command palette finds actions by fuzzy search
5. All documented keyboard shortcuts work
6. `@` autocomplete shows matching files
7. `/` autocomplete shows matching commands

---


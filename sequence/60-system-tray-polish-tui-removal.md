# Step 60 — System Tray, Polish, and TUI Removal

_System tray. Desktop notifications. Theme toggle. Keyboard shortcuts. Empty/loading/error states using prompt-kit. Remove moa-tui. Final type safety audit._

---

## 1. What this step is about

Final polish: desktop integration, visual refinements, and TUI removal. Also includes a final audit to verify all types flow through ts-rs generated bindings.

---

## 2. Files/directories to read

- **`moa-tui/`** — Being removed entirely.
- **`moa-cli/src/main.rs`** — Update to remove TUI dependency.
- **`Cargo.toml` (workspace)** — Remove moa-tui.
- **`package.json`** — Verify clean state.
- **`src/lib/bindings/`** — Generated types from Step 57. Verify all frontend imports point here.
- **`src/router.tsx`** — TanStack Router. Verify all routes work.
- Tauri docs: https://v2.tauri.app/learn/system-tray/ and https://v2.tauri.app/plugin/notification/

Prompt-kit components for polish:
- **`src/components/prompt-kit/loader.tsx`** — Loading skeleton states.
- **`src/components/prompt-kit/prompt-suggestion.tsx`** — Empty session welcome chips.
- **`src/components/prompt-kit/system-message.tsx`** — Error boundaries.
- **`src/components/prompt-kit/text-shimmer.tsx`** — Loading text animations.

---

## 3. Goal

After this step:
1. System tray icon with session status
2. Desktop notifications for approval requests
3. Dark/light theme toggle
4. All keyboard shortcuts working
5. `moa-tui` deleted
6. Empty/loading/error states use prompt-kit components
7. **No hand-written DTO types exist anywhere** — all come from `@/lib/bindings`
8. **No `src/lib/types.ts`** — confirmed deleted in Step 57, stays deleted

---

## 4. Rules

- **All new files kebab-case.**
- **No react-resizable-panels or react-router-dom.** Remove remnants.
- **TanStack Router only.**
- **All DTO type imports from `@/lib/bindings`** (ts-rs generated). No exceptions.
- **Use prompt-kit components** for loading/empty/error states.

---

## 5. Tasks

### 5a. System tray — Tauri `tray-icon` feature
### 5b. Desktop notifications — `@tauri-apps/plugin-notification`
### 5c. Theme toggle — `theme-toggle.tsx` with Sun/Moon icons

### 5d. Keyboard shortcuts — `use-keyboard-shortcuts.ts`

| Shortcut | Action |
|----------|--------|
| Cmd+N | New session |
| Cmd+K | Command palette |
| Cmd+B | Toggle sidebar |
| Cmd+I | Toggle info panel |
| Cmd+, | Settings |
| Cmd+W | Close current tab |
| Ctrl+Tab | Next tab |
| Y / A / N | Approval shortcuts |
| Escape | Cancel / close overlay |

### 5e. Empty states with prompt-kit

```tsx
// Empty session
<PromptSuggestion label="Write a script" description="Create a Python hello world" />
<PromptSuggestion label="Search the web" description="What's the latest AI news?" />

// Loading
<TextShimmer>Loading sessions...</TextShimmer>

// Error recovery
<SystemMessage variant="error">Something went wrong. <Button onClick={retry}>Retry</Button></SystemMessage>
```

### 5f. Remove moa-tui

1. Delete `moa-tui/` directory entirely
2. Remove from workspace `Cargo.toml`
3. Update `moa-cli` to remove TUI dependency

### 5g. Final type safety audit

This is the most important verification in this step:

```bash
# 1. No hand-written types file
test ! -f src/lib/types.ts && echo "PASS: types.ts deleted"

# 2. No imports from the deleted file
grep -r "from.*@/lib/types" src/ --include="*.ts" --include="*.tsx" | wc -l
# Expected: 0

# 3. Bindings exist and are up-to-date
cargo test -p moa-app export_bindings
git diff --exit-code src/lib/bindings/
# Expected: no diff (bindings committed and current)

# 4. TypeScript compiles with generated types
npm run build
# Expected: success

# 5. No react-router-dom or react-resizable-panels
grep "react-router-dom\|react-resizable-panels" package.json | wc -l
# Expected: 0

# 6. All source files kebab-case
find src/ -name "*.tsx" -o -name "*.ts" | grep "[A-Z]" | grep -v "node_modules\|bindings"
# Expected: only generated binding files (PascalCase is fine for ts-rs output)
```

### 5h. CI recommendation (document, don't implement)

Add a note to the README or a CI config suggestion:

```yaml
# In CI, verify generated bindings are committed and current
- name: Check ts-rs bindings
  run: |
    cargo test -p moa-app export_bindings
    git diff --exit-code src/lib/bindings/ || (echo "ERROR: ts-rs bindings are stale. Run 'cargo test -p moa-app' and commit." && exit 1)
```

---

## 6. Deliverables

- [ ] System tray in `src-tauri/src/main.rs`
- [ ] `src/components/theme-toggle.tsx`
- [ ] `src/hooks/use-keyboard-shortcuts.ts`
- [ ] Empty states using prompt-kit components
- [ ] `moa-tui/` — **Deleted**
- [ ] `moa-cli/` — Updated
- [ ] App icons in `src-tauri/icons/`
- [ ] Type safety audit passed (all checks in 5g)

---

## 7. Acceptance criteria

1. System tray visible and functional.
2. Notifications for approval requests.
3. Dark/light theme toggle.
4. All keyboard shortcuts work.
5. Empty sessions show PromptSuggestion chips.
6. Loading states use Loader / TextShimmer.
7. Error states use SystemMessage.
8. `moa-tui/` no longer exists.
9. `cargo build -p moa-cli` compiles.
10. `cargo tauri build` produces distributable app.
11. **`src/lib/types.ts` does not exist.**
12. **Zero imports from `@/lib/types` in any frontend file.**
13. **All DTO type imports come from `@/lib/bindings`.**
14. **`npm run build` succeeds with ts-rs generated types.**
15. No `react-router-dom` or `react-resizable-panels` in `package.json`.
16. All source files kebab-case.

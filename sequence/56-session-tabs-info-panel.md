# Step 56 — Session Tabs + Info Panel

_Tab bar for concurrent sessions. Session info sidebar with cost, tokens, duration, context window visualization, and tools-used list._

---

## 1. What this step is about

Enable working with multiple sessions simultaneously via a tab bar, and flesh out the detail panel (right sidebar) with real session metadata, resource usage, and active tool information.

---

## 2. Files/directories to read

- **`src-tauri/src/commands.rs`** — `get_session`, `list_sessions`, `get_session_events` commands.
- **`src-tauri/src/dto.rs`** — `SessionMetaDto` with all metadata fields.
- **`moa-core/src/types.rs`** — `SessionMeta`, `ModelCapabilities` for context window info.
- **`src/components/layout/app-layout.tsx`** — Current three-column flex layout.
- **`src/components/layout/detail-panel.tsx`** — Placeholder right panel.
- **`src/stores/session.ts`** — Current session store.
- **`src/router.tsx`** — TanStack Router definitions.

Prompt-kit components to use:
- **`src/components/prompt-kit/loader.tsx`** — For loading states in the info panel.

shadcn/ui components: `Badge`, `Card`, `Progress`, `Tooltip`, `ScrollArea`, `DropdownMenu`, `Table`.

---

## 3. Goal

After this step:
1. Tab bar at the top of the chat area shows open sessions
2. Tabs show session name + status icon + close button
3. Drag to reorder tabs
4. Overflow: scrollable with dropdown
5. Right panel shows: duration, turn count, token usage, cost, context window bar, tools-used list

---

## 4. Rules

- **All new files use kebab-case naming.**
- **No react-resizable-panels.** Detail panel remains conditional flex div (`w-[300px]`).
- **Use TanStack Router** for tab click navigation.
- **Use shadcn/ui components** for all UI.
- **Drag-to-reorder uses `@dnd-kit/sortable`.**

---

## 5. Tasks

### 5a. Create Zustand store for open tabs (`src/stores/tabs.ts`)
### 5b. Create `session-tab-bar.tsx` with @dnd-kit/sortable
### 5c. Create `session-info-panel.tsx` replacing detail-panel.tsx placeholder
### 5d. Create `context-window-bar.tsx` — segmented CSS progress bar
### 5e. Create `use-session-meta.ts` hook
### 5f. Integrate tab bar into app-layout.tsx

---

## 6. Deliverables

- [ ] `src/stores/tabs.ts`
- [ ] `src/components/layout/session-tab-bar.tsx`
- [ ] `src/components/layout/session-info-panel.tsx`
- [ ] `src/components/layout/context-window-bar.tsx`
- [ ] `src/hooks/use-session-meta.ts`
- [ ] `src/components/layout/app-layout.tsx` — Updated
- [ ] Dependencies: `@dnd-kit/core`, `@dnd-kit/sortable`, `@dnd-kit/utilities`

---

## 7. Acceptance criteria

1. Opening a session adds a tab. Clicking another adds another tab.
2. Closing a tab switches to next/previous.
3. Dragging a tab reorders it.
4. Info panel shows accurate token/cost data.
5. Context window bar updates in real time during streaming.
6. Ctrl+Tab cycles through open tabs.
7. Tab overflow handles gracefully with scroll/dropdown.

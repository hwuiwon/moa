# Step 53 — Layout Shell + Session Sidebar + Navigation

_Three-column responsive layout. Session list with status indicators, search, and date grouping. React Router navigation between Chat, Memory, and Settings views._

---

## 1. What this step is about

Build the application skeleton that all other views live inside: a collapsible session sidebar on the left, a main content area in the center, and a collapsible detail panel on the right. Wire up React Router for navigating between Chat, Memory, and Settings views. Implement the session sidebar with real data from the backend.

---

## 2. Files/directories to read

- **`src-tauri/src/commands.rs`** — `list_sessions`, `create_session`, `get_session` commands (from Step 52).
- **`src-tauri/src/dto.rs`** — `SessionSummaryDto` with id, title, status, model, updated_at, cost.
- **`moa-core/src/types.rs`** — `SessionStatus` enum, `SessionSummary` struct. Understand what data is available per session.

---

## 3. Goal

After this step, the app shows:
- A left sidebar (260px, collapsible with Cmd+B) listing all sessions with status dots, grouped by date
- A center area that renders the active view (Chat placeholder for now)
- A right panel (300px, collapsible with Cmd+I) for session info (placeholder)
- A top bar with workspace name, model selector, and new-session button
- React Router routes: `/chat/:sessionId`, `/memory`, `/settings`

---

## 4. Rules

- **Use shadcn/ui components** for all UI elements: Button, Input, ScrollArea, Badge, Separator, Tooltip.
- **Tailwind CSS v4** for all styling. No CSS modules, no styled-components.
- **Zustand store** for layout state: sidebar visibility, active session ID, active view.
- **TanStack Query** for fetching session list from backend. Auto-refetch when sessions change.
- **Dark mode as default.** Use CSS variables from shadcn/ui's dark theme. Support light mode toggle later.
- **Keyboard shortcuts:** Cmd+B toggle sidebar, Cmd+N new session, Cmd+K command palette (placeholder).
- **Session list is virtualized** if > 50 sessions. Use `@tanstack/react-virtual`.

---

## 5. Tasks

### 5a. Create Zustand stores

```typescript
// src/stores/layout.ts
interface LayoutStore {
  sidebarOpen: boolean;
  detailPanelOpen: boolean;
  toggleSidebar: () => void;
  toggleDetailPanel: () => void;
}

// src/stores/session.ts
interface SessionStore {
  activeSessionId: string | null;
  setActiveSession: (id: string) => void;
}
```

### 5b. Create the root layout component

```
┌─────────────────────────────────────────────────────────┐
│  TopBar: [≡] MOA  workspace: webapp  model: claude-sonnet│
├────────────┬──────────────────────────┬─────────────────┤
│  Session   │                          │  Detail         │
│  Sidebar   │    <Outlet />            │  Panel          │
│  260px     │    (Router view)         │  300px          │
│  collapsible                          │  collapsible    │
│            │                          │                 │
└────────────┴──────────────────────────┴─────────────────┘
```

Use `react-resizable-panels` for the three columns.

### 5c. Implement session sidebar

- Search input at top (filters by title)
- "New Session" button (calls `invoke("create_session")`)
- Session list grouped by date: "Today", "Yesterday", "Last 7 days", "Older"
- Each row: status dot (colored by SessionStatus), truncated title, relative time, model badge
- Active session highlighted with left border accent
- Click to navigate to `/chat/{sessionId}`
- Right-click context menu: Rename, Delete (shadcn/ui ContextMenu)

### 5d. Implement top bar

- Hamburger menu to toggle sidebar
- "MOA" branding
- Workspace name display
- Model selector dropdown (shadcn/ui Select, populated from config)
- "New Session" button (Cmd+N)
- Settings gear icon → navigates to `/settings`

### 5e. Set up React Router

```typescript
<Routes>
  <Route element={<AppLayout />}>
    <Route path="/chat/:sessionId" element={<ChatView />} />
    <Route path="/memory" element={<MemoryView />} />
    <Route path="/settings" element={<SettingsView />} />
    <Route path="/" element={<Navigate to={`/chat/${defaultSessionId}`} />} />
  </Route>
</Routes>
```

### 5f. Set up TanStack Query for session data

```typescript
function useSessionList() {
  return useQuery({
    queryKey: ['sessions'],
    queryFn: () => invoke<SessionSummaryDto[]>('list_sessions'),
    staleTime: 5000,
  });
}
```

### 5g. Add global keyboard shortcut handler

Listen for Cmd+B, Cmd+N, Cmd+I, Cmd+K at the window level. Use Tauri's global shortcut plugin or React event handlers.

---

## 6. How it should be implemented

Component tree:
```
App
├── QueryClientProvider
├── BrowserRouter
│   └── AppLayout
│       ├── TopBar
│       ├── PanelGroup (react-resizable-panels)
│       │   ├── SessionSidebar
│       │   ├── Outlet (router view)
│       │   └── DetailPanel
│       └── CommandPalette (placeholder)
```

File structure:
```
src/
├── components/
│   ├── layout/
│   │   ├── AppLayout.tsx
│   │   ├── TopBar.tsx
│   │   ├── SessionSidebar.tsx
│   │   └── DetailPanel.tsx
│   └── ui/           # shadcn/ui components
├── stores/
│   ├── layout.ts
│   └── session.ts
├── hooks/
│   └── useSessionList.ts
├── views/
│   ├── ChatView.tsx    # placeholder
│   ├── MemoryView.tsx  # placeholder
│   └── SettingsView.tsx # placeholder
├── lib/
│   ├── tauri.ts        # invoke wrapper with error handling
│   └── utils.ts        # cn() helper, date formatting
├── App.tsx
└── main.tsx
```

---

## 7. Deliverables

- [ ] `src/components/layout/AppLayout.tsx` — Three-column resizable layout
- [ ] `src/components/layout/TopBar.tsx` — App header with model selector
- [ ] `src/components/layout/SessionSidebar.tsx` — Session list with search, grouping, status dots
- [ ] `src/components/layout/DetailPanel.tsx` — Collapsible right panel (placeholder content)
- [ ] `src/stores/layout.ts` + `src/stores/session.ts` — Zustand stores
- [ ] `src/hooks/useSessionList.ts` — TanStack Query hook
- [ ] `src/views/ChatView.tsx` — Placeholder with session ID display
- [ ] `src/views/MemoryView.tsx` — Placeholder
- [ ] `src/views/SettingsView.tsx` — Placeholder
- [ ] `src/App.tsx` — Router setup
- [ ] `src/lib/tauri.ts` — Typed invoke wrapper
- [ ] shadcn/ui components installed (Button, Input, ScrollArea, Badge, Select, ContextMenu, Tooltip, Separator)

---

## 8. Acceptance criteria

1. App launches with three-column layout visible.
2. Session sidebar shows real sessions from the backend, grouped by date.
3. Clicking a session navigates to `/chat/{id}` and highlights it in the sidebar.
4. "New Session" button creates a session and navigates to it.
5. Sidebar collapses/expands with Cmd+B.
6. Model selector dropdown shows available models.
7. Search input filters the session list in real time.
8. Status dots are colored: green=completed, blue=running, amber=waiting_approval, red=failed.

---

## 9. Testing

**Test 1:** Launch app with no sessions → sidebar shows empty state with "New Session" prompt.
**Test 2:** Create 3 sessions → all appear in sidebar sorted by recency.
**Test 3:** Toggle sidebar → content area expands to fill space.
**Test 4:** Search "deploy" → only sessions with "deploy" in title shown.
**Test 5:** Resize sidebar by dragging divider → persists width.
**Test 6:** Cmd+N creates a new session and selects it.

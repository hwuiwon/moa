# Step 54 — Chat View with LLM Streaming

_Migrate to TanStack Router. Message list with streaming markdown rendering. Token-by-token display via Tauri Channel. Prompt input with submit/cancel. Auto-scroll with stick-to-bottom behavior._

---

## 1. What this step is about

Two tasks in one step: first, migrate routing from `react-router-dom` to **TanStack Router** (`@tanstack/react-router`), then build the core chat experience — a scrollable message list that renders user messages and streamed assistant responses in real time, with markdown formatting, code syntax highlighting, and auto-scroll.

The routing migration comes first because every subsequent step depends on the router's `useParams`, `useNavigate`, `Link`, and route definitions.

---

## 2. Files/directories to read

- **`src/App.tsx`** — Current `createHashRouter` from react-router-dom. Needs full replacement.
- **`src/components/layout/app-layout.tsx`** — Uses `Outlet`, `useNavigate`, `useLocation` from react-router-dom. All must switch to TanStack Router equivalents.
- **`src/components/layout/session-sidebar.tsx`** — May use `useNavigate` or `<Link>`.
- **`src/components/layout/top-bar.tsx`** — May use navigation helpers.
- **`src/views/chat-view.tsx`** — Currently a placeholder using `useParams` from react-router-dom.
- **`src/views/memory-view.tsx`** — Placeholder.
- **`src/views/settings-view.tsx`** — Placeholder.
- **`package.json`** — Has `react-router-dom` in dependencies. Also has `react-resizable-panels` which should be removed.
- **`src-tauri/src/commands.rs`** — `send_message` command with `Channel<StreamEvent>` (from Step 52).
- **`src-tauri/src/stream.rs`** — `StreamEvent` variants.
- **`moa-core/src/types.rs`** — `RuntimeEvent` enum for reference.

---

## 3. Goal

After this step:
1. **TanStack Router replaces react-router-dom entirely.** `react-router-dom` is removed from `package.json`.
2. **`react-resizable-panels` is removed from `package.json`** and `src/components/ui/resizable.tsx` is deleted. All layout uses CSS flex/grid and shadcn/ui components only.
3. User types a message → it appears instantly as a user message.
4. Assistant response streams token by token with live markdown rendering.
5. Code blocks render with syntax highlighting (Shiki).
6. Auto-scroll follows new content; pauses when user scrolls up; resumes on scroll-to-bottom.
7. Loading previous session history works (load events on session switch).
8. Cancel button stops the current generation.

---

## 4. Rules

### Routing migration
- **Use `@tanstack/react-router` with code-based route definitions.** Create a `src/router.tsx` file. Prefer code-based routes for this project since it's a Tauri app (no SSR).
- **Use `createHashHistory`** for Tauri compatibility (same as the current hash router). TanStack Router supports this via `createRouter({ history: createHashHistory() })`.
- **Replace all react-router-dom imports:** `useNavigate` → TanStack's `useNavigate`, `useParams` → TanStack's `Route.useParams()`, `Outlet` → TanStack's `Outlet`, `Navigate` → TanStack's `Navigate`, `Link` → TanStack's `Link`.
- **Route tree:** Define routes with `createRootRoute`, `createRoute`. The root route renders `AppLayout`. Child routes: `/chat/$sessionId`, `/memory`, `/settings`, and an index route that redirects.
- **All new files must be kebab-case.** E.g., `router.tsx`, not `Router.tsx`.

### Layout cleanup
- **Remove `react-resizable-panels` from `package.json`.** Delete `src/components/ui/resizable.tsx`.
- **All layout uses CSS flex/grid + Tailwind classes.** The current `app-layout.tsx` already uses this pattern (flex divs with `w-[260px]`, `flex-1`, `w-[300px]`). Keep it.
- For any future split-pane needs (e.g., memory browser), use CSS flex with fixed widths and conditional rendering.

### Chat implementation
- **RAF-batched token accumulation.** Never `setState` on every individual token. Accumulate in a `useRef`, flush via `requestAnimationFrame`. Target 60fps updates.
- **Memoize completed messages.** Only the currently-streaming message re-renders on each token. Completed messages are wrapped in `React.memo`.
- **Use react-markdown + remark-gfm** for markdown rendering. For streaming messages, re-render only the active message's markdown on each RAF flush.
- **Shiki for code highlighting.** Load grammars lazily. Show the code container immediately when a fence opens, stream code content, highlight progressively.
- **Virtualize long conversations.** Use `@tanstack/react-virtual` for sessions with 100+ messages. Short sessions render directly.
- **Flat message styling, not bubbles.** User messages get a subtle background tint. Assistant messages render full-width. Role labels ("You" / "MOA") distinguish sender.

---

## 5. Tasks

### 5a. Install TanStack Router, remove react-router-dom and react-resizable-panels

```bash
npm install @tanstack/react-router
npm uninstall react-router-dom react-resizable-panels
rm src/components/ui/resizable.tsx
```

### 5b. Create route tree

Create `src/router.tsx`:

```typescript
import { createRootRoute, createRoute, createRouter, createHashHistory } from '@tanstack/react-router';
import { AppLayout } from '@/components/layout/app-layout';
import { ChatView } from '@/views/chat-view';
import { MemoryView } from '@/views/memory-view';
import { SettingsView } from '@/views/settings-view';

const rootRoute = createRootRoute({ component: AppLayout });

const indexRoute = createRoute({ getParentRoute: () => rootRoute, path: '/' });
const chatRoute = createRoute({ getParentRoute: () => rootRoute, path: '/chat/$sessionId', component: ChatView });
const chatIndexRoute = createRoute({ getParentRoute: () => rootRoute, path: '/chat', component: ChatView });
const memoryRoute = createRoute({ getParentRoute: () => rootRoute, path: '/memory', component: MemoryView });
const settingsRoute = createRoute({ getParentRoute: () => rootRoute, path: '/settings', component: SettingsView });

const routeTree = rootRoute.addChildren([indexRoute, chatRoute, chatIndexRoute, memoryRoute, settingsRoute]);

export const router = createRouter({ routeTree, history: createHashHistory() });

// Register the router for type safety
declare module '@tanstack/react-router' {
  interface Register { router: typeof router }
}
```

### 5c. Update App.tsx

```typescript
import { RouterProvider } from '@tanstack/react-router';
import { router } from '@/router';

function App() {
  return (
    <TooltipProvider delay={150}>
      <RouterProvider router={router} />
    </TooltipProvider>
  );
}
```

### 5d. Update app-layout.tsx

Replace all `react-router-dom` imports:
- `Outlet` → `import { Outlet } from '@tanstack/react-router'`
- `useNavigate` → `import { useNavigate } from '@tanstack/react-router'`
- `useLocation` → `import { useRouterState } from '@tanstack/react-router'` (use `useRouterState({ select: s => s.location })`)
- `Navigate` → `import { Navigate } from '@tanstack/react-router'`

Navigation calls change: `navigate('/chat/${id}')` → `navigate({ to: '/chat/$sessionId', params: { sessionId: id } })`.

### 5e. Update HomeRedirect

Replace `<Navigate replace to={...} />` with TanStack Router's `<Navigate to="/chat/$sessionId" params={{ sessionId }} />`.

### 5f. Create message data model

```typescript
// src/types/chat.ts
export interface ChatMessage {
  id: string;
  role: 'user' | 'assistant';
  content: string;
  timestamp: string;
  isStreaming: boolean;
  tokens?: { input: number; output: number };
  cost?: number;
  duration?: number;
}
```

### 5g. Create `use-chat-stream.ts` hook

```typescript
// src/hooks/use-chat-stream.ts
export function useChatStream(sessionId: string) {
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const streamRef = useRef('');
  const rafRef = useRef<number>();

  const sendMessage = async (prompt: string) => {
    // Add user message immediately
    // Create Channel<StreamEvent>
    // Call invoke("send_message", { sessionId, prompt, onEvent: channel })
    // Handle StreamEvent variants:
    //   assistantStarted → add empty assistant message (isStreaming: true)
    //   assistantDelta → accumulate in ref, RAF flush to state
    //   assistantFinished → finalize message (isStreaming: false)
    //   error → show error state
  };

  return { messages, sendMessage, isStreaming };
}
```

### 5h. Create chat components

All files kebab-case in `src/components/chat/`:

```
src/components/chat/
├── message-list.tsx        # Scrollable container with auto-scroll
├── user-message.tsx        # User message with subtle bg tint
├── assistant-message.tsx   # Markdown-rendered assistant message
├── prompt-input.tsx        # Multi-line textarea + send/stop buttons
├── streaming-content.tsx   # Wrapper for streaming markdown
└── code-block.tsx          # Shiki-highlighted code with copy button
```

### 5i. Create `use-session-history.ts` hook

```typescript
// src/hooks/use-session-history.ts
export function useSessionHistory(sessionId: string | undefined) {
  return useQuery({
    queryKey: ['session-events', sessionId],
    queryFn: () => invoke('get_session_events', { sessionId }),
    enabled: !!sessionId,
    select: (events) => eventsToMessages(events),
  });
}
```

### 5j. Update chat-view.tsx

Use TanStack Router's route-specific params:

```typescript
import { useParams } from '@tanstack/react-router';

export function ChatView() {
  const { sessionId } = useParams({ strict: false });
  // ...
}
```

Assemble `MessageList` + `PromptInput` into the full view.

---

## 6. How it should be implemented

Component tree for the chat view:
```
ChatView
├── MessageList
│   ├── UserMessage (memoized)
│   ├── AssistantMessage (memoized when not streaming)
│   ├── AssistantMessage (streaming — re-renders on RAF flush)
│   └── ScrollAnchor
├── PromptInput
│   ├── Textarea
│   ├── SendButton / StopButton
│   └── ModelInfo
```

File structure additions:
```
src/
├── router.tsx              # TanStack Router setup (NEW)
├── types/
│   └── chat.ts             # ChatMessage, ContentBlock types
├── components/
│   └── chat/
│       ├── message-list.tsx
│       ├── user-message.tsx
│       ├── assistant-message.tsx
│       ├── prompt-input.tsx
│       ├── streaming-content.tsx
│       └── code-block.tsx
├── hooks/
│   ├── use-chat-stream.ts
│   └── use-session-history.ts
```

---

## 7. Deliverables

- [ ] `react-router-dom` removed from `package.json`
- [ ] `react-resizable-panels` removed from `package.json`
- [ ] `src/components/ui/resizable.tsx` deleted
- [ ] `@tanstack/react-router` installed
- [ ] `src/router.tsx` — Route tree with hash history
- [ ] `src/App.tsx` — Updated to use TanStack RouterProvider
- [ ] `src/components/layout/app-layout.tsx` — Updated imports to TanStack Router
- [ ] `src/components/layout/session-sidebar.tsx` — Updated navigation calls
- [ ] `src/components/layout/top-bar.tsx` — Updated navigation calls
- [ ] `src/types/chat.ts` — Message types
- [ ] `src/components/chat/message-list.tsx`
- [ ] `src/components/chat/user-message.tsx`
- [ ] `src/components/chat/assistant-message.tsx`
- [ ] `src/components/chat/prompt-input.tsx`
- [ ] `src/components/chat/streaming-content.tsx`
- [ ] `src/components/chat/code-block.tsx`
- [ ] `src/hooks/use-chat-stream.ts`
- [ ] `src/hooks/use-session-history.ts`
- [ ] `src/views/chat-view.tsx` — Full implementation
- [ ] Dependencies installed: `@tanstack/react-router`, `react-markdown`, `remark-gfm`, `shiki`, `use-stick-to-bottom`

---

## 8. Acceptance criteria

1. `react-router-dom` is NOT in `package.json`. `react-resizable-panels` is NOT in `package.json`.
2. All routes work via TanStack Router with hash history: `#/chat/{id}`, `#/memory`, `#/settings`.
3. Sidebar navigation still works (clicking a session navigates correctly).
4. Cmd+N, Cmd+B, Cmd+I keyboard shortcuts still work.
5. Typing a message and pressing Enter sends it; user message appears instantly.
6. Assistant response streams token-by-token with visible character accumulation.
7. Markdown renders correctly: headings, bold, italic, lists, links.
8. Code blocks show with syntax highlighting and language label.
9. Auto-scroll follows streaming content.
10. Scrolling up pauses auto-scroll; "Jump to bottom" button appears.
11. Stop button cancels generation mid-stream.
12. Switching sessions loads correct history.
13. No visible jank during streaming (60fps target).
14. Empty session shows a welcome/prompt state.

---

## 9. Testing

**Test 1:** Navigate to `#/chat/{sessionId}` → chat view renders. Navigate to `#/memory` → memory view renders.
**Test 2:** Click session in sidebar → URL updates, chat view shows correct session.
**Test 3:** Send "What is Rust?" → streamed response with markdown renders correctly.
**Test 4:** Send a prompt that produces a code block → syntax highlighting applied.
**Test 5:** Scroll up during streaming → auto-scroll pauses, "Jump to bottom" appears.
**Test 6:** Click "Stop" during streaming → generation stops, partial response preserved.
**Test 7:** Switch sessions → history loads, no residual streaming state from previous session.
**Test 8:** Send 50 messages in one session → virtualization activates, smooth scrolling.

---

## 10. Additional notes

- **Why migrate to TanStack Router now?** It must happen before building real views because every view uses `useParams`, `useNavigate`, etc. Doing it later would mean touching every file twice.
- **TanStack Router advantages for this project:** Type-safe route params (`$sessionId` is typed), built-in search params management, better data loading patterns with `loader`, and consistent with the TanStack Query ecosystem already in use.
- **Hash history is required for Tauri** because the webview serves static files from a custom protocol (`tauri://`), not a real HTTP server. Path-based routing would require catch-all server config that doesn't exist.
- **No react-resizable-panels anywhere.** The current flex-based layout in `app-layout.tsx` (conditional divs with fixed widths) is the correct pattern. For any future split-pane needs, use CSS flex with percentage widths or a lightweight custom drag handle.

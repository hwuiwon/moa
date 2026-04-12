# Step 55 — Tool Visibility + prompt-kit Integration

_Fix the silent-tool-call bug. Extend the message model to ContentBlock[]. Wire toolUpdate, approvalRequired, and notice StreamEvents to prompt-kit's Tool, ThinkingBar, Reasoning, and Steps components._

---

## 1. What this step is about

There is a **critical visibility bug**: when the agent calls tools (e.g., `web_search` for "tell me about the latest news"), the user sees nothing — no spinner, no tool card, no progress indicator — until the final response appears. The cause is in `src/hooks/use-chat-stream.ts`: the `Channel.onmessage` handler has a `default: break` that silently drops `toolUpdate`, `approvalRequired`, `notice`, and `turnCompleted` events.

This step fixes that by:
1. Extending `ChatMessage` from a flat `content: string` to `blocks: ContentBlock[]`
2. Handling ALL `StreamEvent` variants in `use-chat-stream.ts`
3. Rendering tool calls, thinking states, and notices using **installed prompt-kit components**

---

## 2. Files/directories to read

Backend (understand what events are sent):
- **`src-tauri/src/stream.rs`** — `StreamEvent` enum. All variants. The `ToolUpdate` and `ApprovalRequired` variants are already being sent by the backend but **dropped by the frontend**.
- **`src-tauri/src/commands.rs`** — `send_message` function. Shows the `while let Some(event) = event_rx.recv().await` loop that forwards ALL RuntimeEvents to the channel. The backend IS sending these events.
- **`moa-core/src/types.rs`** — `RuntimeEvent`, `ToolUpdate`, `ToolCardStatus`, `ApprovalPrompt`, `RiskLevel`.

Frontend (what needs fixing):
- **`src/hooks/use-chat-stream.ts`** — The bug is here. In the `switch (event.event)` block: `toolUpdate`, `approvalRequired`, `notice`, `turnCompleted` all fall through to `default: break`. **This is the root cause of the blank screen.**
- **`src/types/chat.ts`** — Current `ChatMessage` has `content: string`. Needs `blocks: ContentBlock[]`.
- **`src/components/chat/assistant-message.tsx`** — Currently renders `message.content` as markdown. Needs to render `message.blocks` via a content block renderer.
- **`src/components/chat/message-list.tsx`** — Renders messages. May need updates.

Installed prompt-kit components (USE THESE — do not build from scratch):
- **`src/components/prompt-kit/tool.tsx`** — `<Tool toolPart={...}>` with states: `input-streaming`, `input-available`, `output-available`, `output-error`. Collapsible card with status icon, badge, input/output display.
- **`src/components/prompt-kit/thinking-bar.tsx`** — `<ThinkingBar text="Thinking" onStop={...}>` shimmer text + stop button.
- **`src/components/prompt-kit/reasoning.tsx`** — `<Reasoning isStreaming={...}>` with `<ReasoningTrigger>` and `<ReasoningContent markdown>`. Auto-opens during streaming, auto-closes when done.
- **`src/components/prompt-kit/steps.tsx`** — `<Steps>` with `<StepsTrigger>` + `<StepsContent>` + `<StepsItem>`. Collapsible step list with vertical bar.
- **`src/components/prompt-kit/text-shimmer.tsx`** — `<TextShimmer>` for shimmer animation.
- **`src/components/prompt-kit/system-message.tsx`** — System-level notices/warnings.
- **`src/components/prompt-kit/feedback-bar.tsx`** — Thumbs up/down on completed messages.
- **`src/components/prompt-kit/message.tsx`** — `<Message>` with `<MessageAvatar>`, `<MessageContent>`, `<MessageActions>`.

---

## 3. Goal

After this step:
1. **Tool calls are visible immediately.** When `web_search` runs, a prompt-kit `<Tool>` card appears showing spinner + "Processing" badge + tool name.
2. **Tool completion shows results.** Card updates to "Completed" with expandable output.
3. **Thinking state shows shimmer.** `<ThinkingBar>` shows animated "Thinking..." between request and first token.
4. **Notices are visible.** Runtime notices render as `<SystemMessage>` inline.
5. **Multiple sequential tools group in Steps.** 3+ tools without intervening text wrap in `<Steps>`.
6. The user NEVER sees a blank screen with no feedback while the agent works.

---

## 4. Rules

- **All new files use kebab-case naming.**
- **USE the installed prompt-kit components.** Do NOT recreate tool cards, thinking bars, or step lists from scratch. Import from `@/components/prompt-kit/...`.
- **The `Tool` component from prompt-kit is the primary tool visualization.** Map MOA's tool status to prompt-kit's `ToolPart.state`:
  - `status: "pending"` → `state: "input-streaming"`
  - `status: "running"` → `state: "input-streaming"`
  - `status: "done"` → `state: "output-available"`
  - `status: "error"` → `state: "output-error"`
- **ThinkingBar shows between AssistantStarted and first AssistantDelta.** Once tokens start flowing, ThinkingBar is removed and replaced with streaming content.
- **No react-resizable-panels.** All layout uses CSS flex/grid.
- **No new routing changes.** TanStack Router is already in place.

---

## 5. Tasks

### 5a. Extend the message model in `src/types/chat.ts`

```typescript
export type ToolStatus = 'pending' | 'running' | 'done' | 'error';

export type ContentBlock =
  | { type: 'text'; text: string }
  | { type: 'thinking' }
  | { type: 'tool-call'; callId: string; toolName: string; status: ToolStatus;
      input?: Record<string, unknown>; output?: Record<string, unknown>;
      errorText?: string; duration?: number }
  | { type: 'approval'; requestId: string; toolName: string; riskLevel: string;
      inputSummary: string; diffPreview?: string; decision?: string }
  | { type: 'notice'; message: string };

export interface ChatMessage {
  id: string;
  role: 'user' | 'assistant';
  blocks: ContentBlock[];  // CHANGED from content: string
  isStreaming: boolean;
  timestamp: string;
  tokens?: { input: number; output: number };
  cost?: number;
  duration?: number;
}
```

### 5b. Fix `use-chat-stream.ts` — handle ALL StreamEvent variants

This is the **critical fix**. Update the `channel.onmessage` handler:

```typescript
case 'assistantStarted':
  appendThinkingBlock(runId);
  break;

case 'assistantDelta':
  removeThinkingBlock(runId);
  appendAssistantPlaceholder(runId);
  pendingDeltaRef.current += event.data.text;
  scheduleFlush(runId);
  break;

case 'toolUpdate':
  upsertToolBlock(runId, {
    callId: event.data.callId,
    toolName: event.data.toolName,
    status: event.data.status as ToolStatus,
    summary: event.data.summary,
  });
  break;

case 'approvalRequired':
  addApprovalBlock(runId, event.data);
  break;

case 'notice':
  addNoticeBlock(runId, event.data.message);
  break;

case 'turnCompleted':
  break;
```

Key helper functions to add:
- `appendThinkingBlock(runId)`: Adds `{ type: 'thinking' }` block to assistant message
- `removeThinkingBlock(runId)`: Removes thinking block once real content starts
- `upsertToolBlock(runId, data)`: Finds existing tool block by `callId` and updates, OR inserts new one
- `addApprovalBlock(runId, data)`: Appends an approval block
- `addNoticeBlock(runId, message)`: Appends a notice block

### 5c. Create `content-block-renderer.tsx`

Dispatches each `ContentBlock` to the correct prompt-kit component:

```tsx
import { Tool, type ToolPart } from '@/components/prompt-kit/tool';
import { ThinkingBar } from '@/components/prompt-kit/thinking-bar';
import { SystemMessage } from '@/components/prompt-kit/system-message';
import { StreamingContent } from '@/components/chat/streaming-content';

function ContentBlockRenderer({ block, onStop, onApproval }: Props) {
  switch (block.type) {
    case 'text':
      return <StreamingContent content={block.text} />;
    case 'thinking':
      return <ThinkingBar text="Thinking" onStop={onStop} />;
    case 'tool-call':
      return <Tool toolPart={mapToToolPart(block)} />;
    case 'approval':
      return <ApprovalCard {...block} onDecision={onApproval} />;
    case 'notice':
      return <SystemMessage>{block.message}</SystemMessage>;
  }
}

function mapToToolPart(block: ToolCallBlock): ToolPart {
  const stateMap: Record<ToolStatus, ToolPart['state']> = {
    pending: 'input-streaming',
    running: 'input-streaming',
    done: 'output-available',
    error: 'output-error',
  };
  return {
    type: block.toolName,
    state: stateMap[block.status],
    input: block.input,
    output: block.output,
    toolCallId: block.callId,
    errorText: block.errorText,
  };
}
```

### 5d. Create `tool-group.tsx` using prompt-kit Steps

When 3+ consecutive tool-call blocks appear, wrap in `<Steps>`:

```tsx
import { Steps, StepsTrigger, StepsContent, StepsItem } from '@/components/prompt-kit/steps';
import { Tool } from '@/components/prompt-kit/tool';
import { Loader2, CheckCircle } from 'lucide-react';

function ToolGroup({ tools, allDone }: { tools: ToolCallBlock[]; allDone: boolean }) {
  return (
    <Steps defaultOpen={!allDone}>
      <StepsTrigger
        leftIcon={allDone
          ? <CheckCircle className="size-4 text-green-500" />
          : <Loader2 className="size-4 animate-spin text-blue-500" />}
      >
        {allDone ? `Used ${tools.length} tools` : `Running ${tools.length} tools...`}
      </StepsTrigger>
      <StepsContent>
        {tools.map(tool => (
          <StepsItem key={tool.callId}>
            <Tool toolPart={mapToToolPart(tool)} />
          </StepsItem>
        ))}
      </StepsContent>
    </Steps>
  );
}
```

### 5e. Update `assistant-message.tsx` to render blocks

Replace single-string rendering with block-based rendering. Group consecutive tool blocks into `<ToolGroup>`.

### 5f. Create `approval-card.tsx`

Compose from shadcn/ui components (Card, Badge, Button) since prompt-kit doesn't have a built-in approval component. Risk-level border colors:
- `"low"` → green left border + "Safe" badge
- `"medium"` → amber left border + "Moderate" badge
- `"high"` → red left border + "Dangerous" badge

Three action buttons: "Allow Once" (default), "Always Allow" (outline), "Deny" (destructive).
Connect to `invoke('respond_to_approval', { requestId, decision })`.

### 5g. Update `eventsToMessages` in `src/types/chat.ts`

The function that converts persisted events into `ChatMessage[]` must produce `blocks: ContentBlock[]` format. `ToolCall`/`ToolResult` events produce tool-call blocks. `BrainResponse` produces text blocks.

### 5h. Add FeedbackBar to completed assistant messages

Use prompt-kit `<FeedbackBar>` at the bottom of completed (non-streaming) assistant messages.

---

## 6. Deliverables

- [ ] `src/types/chat.ts` — Extended with `ContentBlock`, `ToolStatus`, updated `eventsToMessages`
- [ ] `src/hooks/use-chat-stream.ts` — **FIXED**: handles `toolUpdate`, `approvalRequired`, `notice`, `turnCompleted`
- [ ] `src/components/chat/content-block-renderer.tsx` — Maps blocks to prompt-kit components
- [ ] `src/components/chat/tool-group.tsx` — Uses prompt-kit `Steps`
- [ ] `src/components/chat/approval-card.tsx` — Inline approval with risk badges
- [ ] `src/components/chat/assistant-message.tsx` — Updated to render `blocks[]`

---

## 7. Acceptance criteria

1. **Ask "tell me about the latest news from Apr 12, 2026"** → a `<Tool>` card appears immediately showing `web_search` with spinner + "Processing" badge. NO blank screen.
2. When the tool completes, the card updates to green "Completed" badge with expandable results.
3. `<ThinkingBar>` with shimmer animation shows between request and first token.
4. ThinkingBar disappears when text starts streaming.
5. Tool errors show red "Error" badge with error text.
6. Runtime notices appear as `<SystemMessage>` inline.
7. 3+ consecutive tools wrap in `<Steps>` with collapsible group.
8. Approval requests show inline card with risk-level coloring and action buttons.
9. Completed messages show `<FeedbackBar>`.
10. Switching sessions loads history correctly with tool blocks visible.

---

## 8. Testing

**Test 1 (THE BUG):** Ask "tell me about the latest news from Apr 12, 2026" → tool card appears immediately, streams result, then response text follows. No blank screen.
**Test 2:** Ask "What is 2+2?" (no tools) → ThinkingBar shows briefly, then response streams normally.
**Test 3:** Ask agent to edit a file → approval card appears with diff preview and action buttons.
**Test 4:** Multi-step task → multiple tool cards appear, grouped in Steps when 3+.
**Test 5:** Switch to a session that previously used tools → history loads with tool cards visible.
**Test 6:** Force a tool error → red error badge + error text displays.

---

## 9. Additional notes

- **This is a P0 bug fix.** The tool visibility gap is the most jarring UX issue right now. Every other improvement is secondary until users can see what the agent is doing.
- **The prompt-kit `Tool` component is feature-complete for this.** Its four-state model maps exactly to MOA's `ToolCardStatus`. Don't reinvent.
- **The `blocks: ContentBlock[]` model is additive.** For simple text-only messages: `[{ type: 'text', text: '...' }]`. For tool-heavy messages: interleaved text and tool blocks. This is the same approach as Vercel AI SDK's message parts.

# Step 14: Telegram Adapter

## What this step is about
Implementing the `PlatformAdapter` for Telegram using `teloxide`.

## Files to read
- `docs/03-communication-layer.md` — Platform capabilities table, Telegram specifics (4096 char limit, InlineKeyboard, edit window)

## Goal
Users can chat with MOA through a Telegram bot. Messages, approvals (inline buttons), tool status updates, and session observation work through Telegram.

## Tasks
1. **`moa-gateway/src/telegram.rs`**: `TelegramAdapter` implementing `PlatformAdapter`. Uses `teloxide` with dptree handler chain.
2. **Message handling**: Incoming text → `InboundMessage`. Parse reply context for threading.
3. **Outbound rendering**: `OutboundMessage` → Telegram format. Markdown formatting, code blocks, message splitting at 4096 chars.
4. **Approval buttons**: `InlineKeyboardMarkup` with `[✅ Allow] [🔁 Always] [❌ Deny]`. Callback handlers map to `ApprovalDecided`.
5. **Status updates**: Edit message in-place every 2-3s during active runs.
6. **Session mapping**: Each MOA session ↔ one Telegram message thread (reply chain).
7. **`moa-gateway/src/renderer.rs`**: Platform-adaptive rendering logic (shared across adapters).

## Deliverables
`moa-gateway/src/telegram.rs`, `moa-gateway/src/renderer.rs`, `moa-gateway/src/lib.rs`

## Acceptance criteria
1. Bot receives messages and routes to orchestrator
2. Responses render with markdown formatting
3. Approval inline buttons appear and work
4. Long messages split correctly at 4096 chars
5. Status message updates in-place during runs
6. Bot token configurable via env var

## Tests
- Unit test: Renderer splits messages at correct character limit
- Unit test: Approval callback data parses correctly
- Integration test (requires bot token): Send message → verify response received
- Mock test: Simulate incoming update → verify correct `InboundMessage` produced

---


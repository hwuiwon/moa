# Step 15: Slack Adapter

## What this step is about
`PlatformAdapter` for Slack using `slack-morphism-rust`.

## Files to read
- `docs/03-communication-layer.md` — Slack specifics (Block Kit, modals, App Home, 40K char limit, threads)

## Tasks
1. **`moa-gateway/src/slack.rs`**: `SlackAdapter` using Socket Mode for events + Web API for sending.
2. **Block Kit rendering**: Approval buttons as `actions` blocks with `primary`/`danger` styles.
3. **Threading**: Each session = a Slack thread. Parent message = status. Replies = event log.
4. **`chat.update`** for in-place status edits (1-2s throttle).
5. **App Home** (deferred to Step 21 polish).

## Deliverables
`moa-gateway/src/slack.rs`

## Acceptance criteria
Same as Telegram but Slack-specific: threads, Block Kit buttons, 40K char limit.

---


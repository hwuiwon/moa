# Channel Adapter Checklist

For implementing `ChannelAdapter` from `moa-core::traits`. Use this when
adding Slack or another enterprise chat channel where users send messages to
MOA and MOA renders responses, action reviews, and progress updates back.

## Trait Surface

The trait lives in `crates/moa-core/src/traits/mod.rs`. It defines channel
identity, capabilities, inbound event startup, send, edit, and delete.

Reference implementation:

- `crates/moa-messaging/src/slack.rs`

Shared rendering/action-review logic lives in:

- `crates/moa-messaging/src/renderer.rs`
- `crates/moa-messaging/src/action_review.rs`

Channel adapter registration is built in
`crates/moa-orchestrator/src/runtime/deps.rs` by the `build_channel_adapters`
map.

## Module Layout

Each channel's code belongs under `crates/moa-messaging/src/`. Start with a
single file such as `slack.rs`; split into a folder only when the adapter grows
large enough to justify separate client, event, and render modules. Optional
SDK dependencies stay behind a feature flag such as `slack`.

## Required Behaviors

1. **Inbound event routing.** Verify the channel-native event, extract message
   text and metadata, and emit a MOA `ChannelEvent` with tenant/contact/session
   routing evidence.
2. **Outbound rendering.** Render MOA responses and progress/status updates in
   the channel's native format.
3. **Action-review rendering.** Render tenant action-review prompts with
   allow/deny controls and handle the resulting action.
4. **Identity and tenant routing.** Resolve channel users/installations to MOA
   tenant/contact identity. Do not derive a MOA identity from an unscoped
   external user id.
5. **Webhook signature verification.** Verify inbound channel requests before
   business logic runs.
6. **Live progress delivery.** Respect the channel's delivery mode and update
   existing status messages when the channel supports edits.

## Test Patterns

- Offline event-normalization test for inbound payloads.
- Signature-verification test that rejects invalid signatures before routing.
- Renderer/action-review tests for native channel payload shape.
- Rate-limit and edit-fallback tests for channels with API pacing.

Live tests against a real chat platform usually need a bot token and deployed
webhook, so keep them ignored and manually gated unless the provider has a
dedicated `MOA_RUN_LIVE_*_TESTS` lane.

## Common Mistakes

- Skipping signature verification during initial wiring.
- Putting channel-specific layout decisions into shared renderer code that
  should stay channel-neutral.
- Treating progress as logs only; chat channels can surface durable progress
  through status updates.
- Forgetting message-size limits and edit fallbacks.

# Platform Adapter Checklist

For implementing `PlatformAdapter` from `moa-core::traits`. Use this when adding Slack or another enterprise chat platform where users send messages to MOA and MOA renders responses, approvals, and observation streams back.

## Trait Surface

The trait lives at `crates/moa-core/src/traits/mod.rs` (function `PlatformAdapter` near line 386). It defines: receive incoming messages, send outgoing messages, render approvals, and stream observation events.

Reference implementation:

- `crates/moa-gateway/src/slack.rs`

The shared rendering logic lives in `crates/moa-gateway/src/renderer.rs`. The shared approval logic lives in `crates/moa-gateway/src/approval.rs`.

## Module Layout

Each platform's code is a single file under `crates/moa-gateway/src/<platform>.rs`. Behind a feature flag named after the platform, such as `slack`. The feature flag controls whether the dependency is compiled into `moa-gateway`.

If the platform implementation grows past ~600 lines, split into a folder: `crates/moa-gateway/src/<platform>/{mod.rs,client.rs,events.rs,render.rs}`.

## Required Behaviors

1. **Inbound message routing.** Receive a platform-native event, extract the user message and metadata, produce a MOA `UserMessage` with `TenantId`, `UserId` or contact identity, and platform-specific identifiers preserved in metadata.
2. **Outbound message rendering.** Take MOA's structured response (text, tool results, observation events) and render to the platform's native format, such as Block Kit for Slack.
3. **Approval rendering.** Render an approval prompt with action buttons. The approval API is shared in `approval.rs`; the platform adapter implements only the rendering and click handling.
4. **Observation streaming.** During a turn, MOA emits observation events (tool call started, tool result, brain thinking). The adapter decides whether to render these inline, in a thread, or via reactions, depending on platform conventions.
5. **Identity resolution.** The platform's user ID maps to MOA's `UserId` or contact identity inside a tenant. Store the mapping in the credential vault or session store; do not derive MOA identity from the platform ID directly (platform IDs may collide across tenants).
6. **Tenant selection.** A user may administer or participate in multiple tenants. The adapter must resolve the tenant from the platform installation, team, channel, or explicit selection. Slack uses team + channel; future adapters must define their tenant routing explicitly.
7. **Webhook signature verification.** Inbound requests from the platform must be verified against the platform's signing secret. The verification step is non-optional; an adapter without signature verification cannot be deployed.

## Renderer Split

The renderer in `crates/moa-gateway/src/renderer.rs` produces a platform-neutral structured response. The adapter consumes that structure and emits platform-native bytes. Do not put platform-specific logic in the renderer; do not put MOA-specific logic in the adapter.

The split:

- **Renderer** (shared): decides what to show (collapse long tool output, hide internal metadata, format error messages).
- **Adapter** (platform-specific): decides how to show it (Block Kit or another native format; threaded vs inline; reactions vs status pills).

If you find yourself adding `if platform == "slack"` to the renderer, the logic belongs in the adapter.

## Feature Flag Gating

The platform's dependencies must be feature-gated:

```toml
[features]
default = ["slack"]
slack = ["dep:slack-morphism"]
```

Default builds compile with Slack as the enterprise messaging adapter. This keeps PR-CI aligned with the supported production surface and avoids dragging in unsupported chat-platform SDKs for unrelated changes.

## Test Patterns

- Offline test that simulates an inbound webhook payload, asserts the produced `UserMessage`, and asserts the rendered outbound payload structure.
- Test for signature verification: a bad signature must return 401/403 before any business logic runs.
- Test for tenant selection: a user with two tenants gets the right one.
- Approval-render test that exercises the AllowOnce / AlwaysAllow / Deny buttons.
- Observation-stream test that covers a multi-tool turn.

Live tests against the real platform are not gated by `MOA_RUN_LIVE_*_TESTS` because they typically require a long-lived bot token and webhook deployment. Mark them `#[ignore]` and run manually before release.

## Wiring Points

1. The gateway feature flag in `crates/moa-gateway/Cargo.toml`.
2. The adapter registry (search `PlatformAdapter` registration in `moa-gateway/src/lib.rs`).
3. The credential vault for the platform's bot token and webhook secret.
4. The hosted gateway or deployment entrypoint for exercising the platform adapter, if one is required.

## Common Mistakes

- Skipping signature verification "for now"; this becomes a deployment blocker.
- Putting Markdown-vs-Block-Kit logic in the renderer; that splits the platform's responsibility across two files.
- Hardcoding the tenant selection rule per platform instead of going through the shared tenant-routing API.
- Treating observation events as text to log; they are first-class UX and should render distinctively.
- Forgetting that long messages need pagination on platforms with size limits, including Slack's message and block limits.

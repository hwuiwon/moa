# Step 16: Discord Adapter + Platform Approval UX

## What this step is about
Discord adapter + finalizing the cross-platform approval rendering that works consistently across all three platforms.

## Files to read
- `docs/03-communication-layer.md` — Discord specifics (ActionRow, embeds, 2K char limit, threads)

## Tasks
1. **`moa-gateway/src/discord.rs`**: `DiscordAdapter` using `serenity`. Auto-create threads per session.
2. **Embeds**: Use Discord embeds for status display (4096 char description). Diff syntax highlighting via `diff` code blocks.
3. **ActionRow buttons**: Approval buttons with styles.
4. **`moa-gateway/src/approval.rs`**: Unified approval rendering that adapts to platform capabilities. `PlatformCapabilities` determines whether to use inline buttons, modals, or text-based prompts.
5. **Post-decision editing**: After approval, edit original message to show result.

## Deliverables
`moa-gateway/src/discord.rs`, `moa-gateway/src/approval.rs`

## Acceptance criteria
1. All three platforms render approvals correctly
2. All three platforms handle session observation with status updates
3. Post-decision message editing works on all platforms
4. Platform capabilities correctly degrade (e.g., no modals on Telegram → use inline buttons instead)

## Tests
- Unit test: Approval renderer produces correct format for each platform
- Unit test: Message truncation at platform-specific limits
- Integration test per platform (requires tokens): Send message, verify response, test approval flow

# Implementation Caveats

Implementation notes and design caveats surfaced while building the current MOA workspace. These are not necessarily bugs, but they are places where the current trait surface or helper behavior is awkward enough to review before later steps build on top of them.

Caveats are grouped by root cause / architectural boundary, not by the crate where the symptom first appears. Fixing the root of a group typically unblocks every caveat in it.

---

## Gateway: platform callbacks are not first-class typed events

All three messaging adapters normalize platform-specific callback payloads into text control messages instead of typed gateway events. The fix is the same everywhere: widen `PlatformAdapter` to emit structured callback events alongside `InboundMessage`.

### Telegram approval callbacks are normalized into control messages

- `moa-gateway/src/telegram.rs` receives Telegram callback queries for approval buttons.
- The core `PlatformAdapter` trait only emits `InboundMessage`; it does not have a direct signal callback surface for `ApprovalDecided`.
- The adapter converts callback payloads into normalized control text such as `/approval allow <request_id>` and forwards that as an `InboundMessage`.
- This works without widening the core gateway trait surface and keeps approval payloads compact and testable.
- A future gateway/orchestrator seam may want a richer event type than `InboundMessage.text` for platform-originated control actions.

### Slack approval buttons are normalized back into control messages

- `moa-gateway/src/slack.rs` receives Block Kit button actions over Socket Mode.
- The core `PlatformAdapter` trait still only emits `InboundMessage`.
- The adapter converts approval button clicks into normalized commands such as `/approval deny <request_id>`.
- Approval actions from Slack and Telegram share the same downstream parsing model.
- If adapters need richer structured callbacks later, `InboundMessage.text` should stop carrying control commands.

### The unified approval layer still targets inline-button platforms first

- `moa-gateway/src/approval.rs` is the single source of truth for approval callback encoding and default approval buttons.
- Telegram, Slack, and Discord all consume that same callback format and button set, with a fallback to text commands when inline buttons are unavailable.
- Approval rendering is consistent across all current messaging adapters.
- The generic gateway surface still has no first-class modal representation. `PlatformCapabilities.supports_modals` is informative today, but the unified approval flow still chooses inline buttons when available and text fallback otherwise.

---

## Gateway: outbound routing requires an inbound anchor

All three adapters resolve outbound destinations from `reply_to` and cannot proactively start conversations. The shared fix is an explicit destination field on `OutboundMessage` or the adapter trait.

### Telegram outbound sends require a reply anchor

- `OutboundMessage` does not carry an explicit destination chat or thread.
- The Telegram adapter resolves where to send by using `reply_to` as an anchor into either a known inbound Telegram message id or a previously sent synthetic gateway message id.
- Reply-chain session mapping works for the intended session model, and the adapter can send, edit, and delete multi-part Telegram replies without changing the existing trait.
- The adapter cannot originate a brand-new top-level Telegram conversation without an existing reply anchor.

### Slack outbound routing depends on an existing reply anchor

- `OutboundMessage` still has no explicit Slack destination.
- `moa-gateway/src/slack.rs` resolves channel/thread targets from `reply_to`, using either a known inbound Slack message timestamp or a previously sent synthetic gateway message id.
- The intended session model works: one MOA session per Slack thread, with replies and edits anchored correctly.
- The adapter cannot proactively open a brand-new channel/thread without a prior inbound anchor.

### Discord thread mapping is anchored to an inbound message

- `moa-gateway/src/discord.rs` auto-creates a thread the first time the adapter responds to a guild message that is not already in a thread. Direct messages stay in the DM channel, and existing threads are reused.
- The "one MOA session per Discord thread" model works for the normal inbound-driven flow.
- Like Telegram and Slack, the Discord adapter relies on `reply_to` as its routing anchor and cannot proactively open a new conversation without an inbound message or prior synthetic gateway message id.

---

## Gateway: conservative rendering

Both Telegram and Slack rendering are intentionally minimal. Upgrading either one requires a proper platform-safe formatting layer with escaping and richer markup.

### Telegram rendering is intentionally conservative

- `moa-gateway/src/renderer.rs` renders text, tool cards, approvals, status updates, diffs, and code blocks.
- Long messages are split at Telegram's 4096-character limit and approval buttons stay on the final chunk.
- Rendering uses plain text plus fenced blocks instead of full Telegram Markdown/HTML parse-mode formatting. This is robust against escaping bugs and message splitting issues.
- If the bot starts carrying heavier user-facing traffic, the next upgrade should be a proper Telegram-safe formatting layer with escaping and richer inline emphasis.

### Slack rendering is intentionally minimal Block Kit

- `moa-gateway/src/renderer.rs` splits Slack output at the 40K text cap and renders approval buttons as Block Kit actions.
- Normal text/code/diff output stays text-first, with Block Kit only added when interactive buttons are needed.
- The adapter uses `chat.update` directly and advertises a 1-second edit interval, but does not yet coalesce bursts of intermediate status updates into a smarter buffer.
- If Slack becomes a primary surface, the next upgrade should add richer per-event thread rendering and more deliberate edit throttling/coalescing.

---

## Security posture — intentional trade-offs

These caveats are deliberate security trade-offs where the current implementation is good enough for the current threat model but has a known upgrade path for stricter requirements.

### MCP remote auth applies only to HTTP/SSE transports

- `moa-security/src/mcp_proxy.rs` issues session-scoped opaque tokens and injects real credentials only when a remote MCP call is dispatched. `moa-hands/src/mcp.rs` supports stdio and remote JSON-RPC transports, with SSE response parsing for remote endpoints.
- Remote MCP servers receive credentials without exposing them to the brain or to serialized tool arguments. Session-scoped auth is enforced at the router/proxy seam.
- Stdio MCP auth is still process/env based; the proxy does not inject credentials into subprocess transports. The credential flow is strongest for HTTP/SSE MCP servers and weaker for stdio servers that need secrets at startup.

### Local encrypted vault uses a generated local passphrase file

- `moa-security/src/vault.rs` stores credentials in `vault.enc` as an age-encrypted JSON document. The local passphrase is generated automatically on first use and stored beside the vault as `vault.key` with `0600` permissions on Unix.
- This keeps credentials encrypted at rest without requiring user setup before the first run.
- This is stronger than plaintext-at-rest, but weaker than OS keychain or external secret-manager storage because the decryption material still lives on the same machine. The swap point is the `CredentialVault` trait.

### Local Docker hardening disables container network access entirely

- `moa-hands/src/local.rs` starts Docker sandboxes with read-only root filesystem, tmpfs scratch mounts, `cap-drop=ALL`, `no-new-privileges:true`, `pids-limit=256`, and Docker seccomp active. The implementation uses `--network none` to block the cloud metadata endpoint.
- This is stricter than the original spec: local Docker sandboxes are fully offline, not just metadata-blocked.
- If we later need outbound network for local containerized tools, we will need a narrower metadata-blocking mechanism than `--network none`.

### Repeated malicious tool loops are still model-driven

- `moa-brain/src/harness.rs` injects a per-turn canary into tool-enabled requests. Tool invocations are blocked if they leak the active canary or any `moa_canary_*` marker. Tool outputs are wrapped in `<untrusted_tool_output>` with an explicit instruction not to follow embedded instructions. Suspicious output produces `Warning` events.
- The instruction hierarchy is materially stronger and regression tests cover both canary leakage and malicious tool-output containment.
- If a model keeps emitting fresh malicious tool calls after seeing the resulting `ToolError`/`Warning`, the retry behavior is still governed by the turn loop rather than a dedicated security circuit breaker. The next seam to tighten is the orchestrator/harness retry policy.

---

## Temporal / cloud orchestration maturity

The Temporal integration works for the current prototype but is not yet production-grade in lifecycle management, child workflow semantics, or scaling topology.

### Temporal approval resume had a real wait-condition bug

- `moa-orchestrator/src/temporal.rs` now gates the workflow loop differently while paused for approval. When `waiting_for_approval` is true, the workflow waits only for an approval decision or cancel request.
- This was a real correctness bug in the initial implementation, not just a missing test. `ApprovalRequested` no longer deadlocks the workflow.
- The Temporal path should keep at least one ignored live dev-server integration test because replay-only or unit tests would not have caught this specific bug.

### Temporal child workflows are not true Temporal child workflows

- `TemporalOrchestrator::spawn_child_workflow()` currently delegates to `start_session()`, creating another top-level session workflow with normal session metadata.
- Sub-brain work can be started as an independent Temporal-backed session, and the public API surface is usable without blocking later cloud work.
- This is not yet using Temporal's actual child-workflow semantics. Parent/child cancellation propagation, parent-close behavior, and Temporal-native child observability are not implemented.

### Worker lifetime is process-scoped and not gracefully stoppable

- `TemporalRuntime::connect()` starts a dedicated OS thread that owns a current-thread Tokio runtime and runs the Temporal worker. The `JoinHandle` is retained only to keep the thread alive.
- The worker polls workflows and activities correctly without violating the SDK's non-`Send` constraints.
- Dropping `TemporalOrchestrator` does not perform a graceful worker shutdown; it detaches the worker thread until process exit. A production deployment likely needs an explicit worker lifecycle manager.

### Cloud deploy runs the local runtime shape with cloud-backed storage

- `moa-cli/src/daemon.rs` exposes a cloud-friendly daemon entrypoint with an HTTP `/health` endpoint and graceful shutdown handling.
- `Dockerfile`, `fly.toml`, and `.github/workflows/deploy.yml` build and launch `moa` with the `cloud` feature set and Fly health-check wiring.
- `moa-session/src/turso.rs` supports libSQL embedded replicas when `cloud.turso_url` is configured.
- This is still the local daemon/runtime shape running in a cloud container, not a dedicated Temporal worker/service topology. A Fly deployment today is effectively "local orchestrator plus cloud-backed session storage."

---

## Architectural drift risks

These caveats are not bugs today but mark places where a future change could silently reintroduce the coupling or duplication that was just cleaned up.

### Tool approval metadata has one source of truth in `ToolRouter`

- Tool normalization, approval summaries, default policy actions, always-allow patterns, and file diff previews all come from the tool definition metadata in `moa-hands/src/router.rs`.
- `moa-security` evaluates policy decisions against a normalized `ToolPolicyInput` prepared by the router. `moa-orchestrator` and `moa-brain` both consume router-prepared invocation metadata.
- Any future non-router execution path must also use `ToolRouter::prepare_invocation()` before applying policy or rendering approval UI. Bypassing the router would reintroduce drift immediately.

### The in-memory skills model matches the on-disk Agent Skills shape

- `SkillFrontmatter` stores only the spec-shaped fields: `name`, `description`, optional `license`, `compatibility`, `allowed-tools`, and `metadata`. MOA bookkeeping is derived lazily from `metadata` through helper accessors.
- The in-memory and on-disk skill representations line up directly, and round-tripping `SKILL.md` is less surprising.
- MOA-specific behavior still depends on `metadata` keys such as `moa-version`, `moa-one-liner`, and `moa-estimated-tokens`. Any code outside `moa-skills` that reaches into raw `metadata` directly is more fragile than code that goes through the helpers in `format.rs`.

### Context stages own their async I/O directly

- `ContextProcessor::process()` is async across `moa-core` and `moa-brain`. Stage 4 (`SkillInjector`), Stage 5 (`MemoryRetriever`), and Stage 6 (`HistoryCompiler`) receive their dependencies through constructor injection and do their own I/O inside `process()`.
- The pipeline runner no longer preloads stage inputs into `WorkingContext.metadata` via stringly-typed JSON keys.
- `WorkingContext.metadata` still exists for legitimate shared request state such as tool schemas. If that map starts accumulating new stage-specific payload contracts again, the repo will drift back toward the same coupling problem.

### Tool metadata lives in `moa-core`, routing stays in `moa-hands`

- `BuiltInTool`, `ToolContext`, `ToolDefinition`, `ToolPolicySpec`, `ToolInputShape`, and `ToolDiffStrategy` live in `moa-core`. `ToolRegistry`, `ToolRouter`, and `ToolExecution` remain in `moa-hands`.
- `moa-hands` re-exports the moved interface types so existing import paths continue to work during the transition.
- `ToolRegistry` still stores execution state separately from the shared `ToolDefinition`, so there are two closely related internal shapes in `moa-hands`. If registry metadata starts drifting from the core definition, the next cleanup should tighten construction helpers around the registry entry type.

---

## TUI feature gaps

The TUI surfaces are functional but intentionally shallow. These caveats share a root in the TUI being a thin consumer layer that defers richer interactions to later passes.

### Memory/settings surfaces are functional but shallow

- `moa-tui/src/app.rs` exposes shells for sidebar, memory browser, settings, help, command palette, slash completion, and `@file` completion.
- The memory browser in `moa-tui/src/views/memory.rs` supports page browsing, FTS-backed search, wikilink following, and back/forward history.
- The settings panel in `moa-tui/src/views/settings.rs` persists a focused subset of config values and hot-reloads the provider/model path.
- The memory browser does not yet implement destructive delete or external-editor open. Markdown rendering is lightweight text, not full `pulldown-cmark` rich formatting. The settings panel intentionally edits a small subset of config rather than every field in `MoaConfig`.

### Prompt completion is intentionally simple

- Slash completion and `@file` completion render above the prompt and accept via `Tab`. File completion is ranked by a small in-memory frecency map plus path order.
- Completion is prompt-text based, not true cursor-position aware editing inside arbitrary multiline input. `@file` completion only rewrites the trailing token, so paths with embedded spaces are not handled. File-frecency is process-local and not persisted across TUI restarts.

---

## Deployment and boot configuration

These caveats relate to the gap between "cloud build succeeds" and "cloud deployment is fully self-service."

### `moa sync enable` validates the session-store path, not the full cloud stack

- `moa-cli/src/main.rs` exposes `moa sync enable`. The command writes cloud sync config only after it can open the embedded replica and perform an initial database sync.
- The command only validates the session-store sync path; it does not validate Fly deployment readiness, memory storage readiness, or platform gateway credentials.
- On this machine, Temporal/cloud-feature builds require `PROTOC=/opt/homebrew/bin/protoc` because the earlier `~/.local/bin/protoc` in `PATH` is not executable.

### Fly deployment requires explicit cloud-hand and provider boot configuration

- `Dockerfile` installs `libprotobuf-dev` in addition to `protobuf-compiler` (required for `prost-wkt-types` standard Google protobuf include files).
- `Dockerfile` and `fly.toml` set `MOA__CLOUD__HANDS__DEFAULT_PROVIDER=local`, so the cloud image can boot without the optional `daytona` or `e2b` features enabled.
- A live Fly deployment to `moa-brains.fly.dev` succeeded after staging `OPENAI_API_KEY`, `MOA__CLOUD__TURSO_URL`, and `TURSO_AUTH_TOKEN` as Fly secrets.
- The daemon constructs the default LLM provider during boot, so health-only startup requires a valid provider API key secret even before the first user request.
- In live validation, a manually stopped machine took roughly 6 seconds to answer the next `/health` request, while a suspended machine resumed in about 1.29 seconds — close to, but not consistently below, the sub-second resume target from the step spec.

# Implementation Caveats

Implementation notes and design caveats surfaced while building the current MOA workspace. These are not necessarily bugs, but they are places where the current trait surface or helper behavior is awkward enough to review before later steps build on top of them.

Caveats are grouped by root cause / architectural boundary, not by the crate where the symptom first appears. Fixing the root of a group typically unblocks every caveat in it.

## 13. Simplification pass

### 13.1 Tool approval metadata now has one source of truth in `ToolRouter`

Current state:

- Tool normalization, approval summaries, default policy actions, always-allow patterns, and file diff previews now come from the tool definition metadata in `moa-hands/src/router.rs`.
- `moa-security` now evaluates policy decisions against a normalized `ToolPolicyInput` prepared by the router instead of re-deriving fields from raw JSON input.
- `moa-orchestrator` and `moa-brain` both consume the router-prepared invocation metadata.

Consequence:

- The earlier drift risk between `moa-security/src/policies.rs` and `moa-orchestrator/src/local.rs` is removed.
- Adding a new tool now requires updating one metadata definition instead of several parallel helper tables.

Remaining caveat:

- Any future non-router execution path must also use `ToolRouter::prepare_invocation()` before applying policy or rendering approval UI. Bypassing the router would reintroduce drift immediately.

### 13.2 Turn execution now has one streamed source of truth

Current state:

- The shared streamed turn engine now lives in `moa-brain/src/harness.rs`, with lower-level stream helpers in `moa-brain/src/turn.rs`.
- `run_brain_turn_with_tools()` is now a buffered wrapper around that streamed engine.
- `LocalOrchestrator` calls the same streamed engine for live sessions and publishes `RuntimeEvent`s from it.
- `BrainOrchestrator` now exposes `observe_runtime()`, and the local TUI runtime is only a stream consumer.

Consequence:

- There is no longer a second TUI-only turn loop to keep in sync with the buffered harness.
- Streaming behavior, tool execution, approval waiting, and durable event persistence now share one implementation.
- `moa exec` and the local TUI both observe the same runtime event stream the orchestrator uses internally.

Remaining caveat:

- `TemporalOrchestrator::observe_runtime()` still returns `None`; cloud/runtime observation needs an explicit transport such as SSE or WebSocket.
- The streamed engine emits ephemeral runtime deltas plus durable session events, so remote observers still need a bridging layer if they cannot subscribe to an in-process broadcast channel.

### 13.3 The in-memory skills model now matches the on-disk Agent Skills shape

Current state:

- `SkillFrontmatter` now stores only the spec-shaped fields:
  - `name`
  - `description`
  - optional `license`
  - optional `compatibility`
  - optional `allowed-tools`
  - optional `metadata`
- MOA bookkeeping is derived lazily from `metadata` through helper accessors instead of being stored twice.
- `tools_required` is gone as a stored field; callers now use `allowed_tools`.

Consequence:

- The old normalization layer in `moa-skills/src/format.rs` is much smaller.
- The in-memory and on-disk skill representations now line up directly.
- Round-tripping `SKILL.md` is less surprising because the runtime preserves the same top-level shape it parses.

Remaining caveat:

- MOA-specific behavior still depends on `metadata` keys such as `moa-version`, `moa-one-liner`, and `moa-estimated-tokens`.
- Any code outside `moa-skills` that reaches into raw `metadata` directly is now more fragile than code that goes through the helpers in `format.rs`.

## 14. Step 13 memory maintenance

### 14.1 Consolidation is heuristic, not yet LLM-driven

Current state:

- `moa-memory/src/consolidation.rs` now performs deterministic maintenance locally:
  - relative date normalization
  - port-claim contradiction resolution
  - pruning entities with `metadata.entity_exists = false`
  - confidence decay for old unreferenced pages
  - orphan detection
  - `MEMORY.md` regeneration and `_log.md` append

Consequence:

- Step 13 is implemented and testable without network or provider availability.
- The behavior is stable and cheap, which is appropriate for local hourly maintenance.

Remaining caveat:

- Consolidation currently understands only a narrow set of contradiction patterns and stale-page signals.
- The implementation matches the architectural role, but not the full eventual intelligence implied by the spec’s LLM-maintainer sketch.

### 14.2 Branch writes exist as a concrete file-store feature, but the runtime still writes directly to mainline memory

Current state:

- `moa-memory/src/branching.rs` provides:
  - branch-local writes under `.branches/`
  - a JSON change manifest
  - deterministic reconciliation back into the main scope
- The live tool/runtime path still uses direct `write_page_in_scope()` / `memory_write`, not branch-local writes.

Consequence:

- Concurrent-write isolation is implemented at the store level and covered by tests.
- The production runtime does not yet automatically route session-specific memory writes through branch directories.

Remaining caveat:

- The branch/reconcile model is available, but not yet wired into the orchestrator or tools as the default write path.
- If true concurrent cloud writers become a near-term requirement, the router/runtime should switch to branched writes instead of direct mainline writes.

### 14.3 Scheduled consolidation currently runs for workspace scopes only

Current state:

- `LocalOrchestrator` now registers an hourly maintenance job that calls `FileMemoryStore::run_due_consolidations(...)`.
- That scheduler currently derives scopes from session history and only executes workspace consolidations.

Consequence:

- Shared project memory now gets the expected periodic maintenance.
- The cron hook stays aligned with the existing local store model and avoids duplicating the known local user-scope ambiguity.

Remaining caveat:

- User-scope consolidation is not scheduled yet.
- This is intentional while local user memory still shares one physical `memory/` root regardless of user id, but it means personal memory does not yet get automatic dream-cycle maintenance.

## 15. Step 14 Telegram gateway

### 15.1 `PlatformAdapter` is still message-oriented, so Telegram callbacks are normalized into control messages

Current state:

- `moa-gateway/src/telegram.rs` receives Telegram callback queries for approval buttons.
- The core `PlatformAdapter` trait only emits `InboundMessage`; it does not have a direct signal callback surface for `ApprovalDecided`.
- The adapter therefore converts callback payloads into normalized control text such as `/approval allow <request_id>` and forwards that as an `InboundMessage`.

Consequence:

- Step 14 works without widening the core gateway trait surface.
- Approval button payloads are compact, testable, and future routing layers can parse them deterministically.

Remaining caveat:

- Telegram approval callbacks are not yet first-class typed gateway events.
- A future gateway/orchestrator seam may want a richer event type than `InboundMessage.text` for platform-originated control actions.

### 15.2 Outbound Telegram sends still require a reply anchor

Current state:

- `OutboundMessage` does not carry an explicit destination chat or thread.
- The Telegram adapter resolves where to send by using `reply_to` as an anchor into either:
  - a known inbound Telegram message id, or
  - a previously sent synthetic gateway message id.

Consequence:

- Reply-chain session mapping works for the intended session model.
- The adapter can send, edit, and delete multi-part Telegram replies without changing the existing trait.

Remaining caveat:

- The adapter cannot originate a brand-new top-level Telegram conversation without an existing reply anchor.
- If gateway adapters eventually need to proactively start conversations, `OutboundMessage` or the adapter trait should grow an explicit destination field.

### 15.3 Telegram rendering is intentionally conservative right now

Current state:

- `moa-gateway/src/renderer.rs` renders text, tool cards, approvals, status updates, diffs, and code blocks.
- Long messages are split at Telegram’s 4096-character limit and approval buttons stay on the final chunk.
- Rendering currently uses plain text plus fenced blocks instead of full Telegram Markdown/HTML parse-mode formatting.

Consequence:

- The renderer is robust against escaping bugs and message splitting issues.
- Code/diff output stays readable and the adapter passed feature-gated tests quickly.

Remaining caveat:

- Rich Telegram-specific formatting is not fully implemented yet.
- If the bot starts carrying heavier user-facing traffic, the next upgrade should be a proper Telegram-safe formatting layer with escaping and richer inline emphasis.

## 16. Step 15 Slack gateway

### 16.1 Slack approval buttons are still normalized back into control messages

Current state:

- `moa-gateway/src/slack.rs` receives Block Kit button actions over Socket Mode.
- The core `PlatformAdapter` trait still only emits `InboundMessage`.
- The adapter converts approval button clicks into normalized commands such as `/approval deny <request_id>`.

Consequence:

- Step 15 works without widening the gateway trait surface.
- Approval actions from Slack and Telegram now share the same downstream parsing model.

Remaining caveat:

- Slack button actions are not yet first-class typed gateway events.
- If adapters need richer structured callbacks later, `InboundMessage.text` should stop carrying control commands.

### 16.2 Slack outbound routing still depends on an existing reply anchor

Current state:

- `OutboundMessage` still has no explicit Slack destination.
- `moa-gateway/src/slack.rs` resolves channel/thread targets from `reply_to`, using either:
  - a known inbound Slack message timestamp, or
  - a previously sent synthetic gateway message id.

Consequence:

- The intended session model works: one MOA session per Slack thread, with replies and edits anchored correctly.
- Status and event-log messages can be posted and updated in-thread.

Remaining caveat:

- The adapter cannot proactively open a brand-new channel/thread without a prior inbound anchor.
- If outbound-initiated Slack workflows are needed, the platform trait or outbound message model must gain an explicit destination.

### 16.3 Slack rendering is intentionally minimal Block Kit right now

Current state:

- `moa-gateway/src/renderer.rs` now splits Slack output at the 40K text cap and renders approval buttons as Block Kit actions.
- Normal text/code/diff output stays text-first, with Block Kit only added when interactive buttons are needed.
- The adapter uses `chat.update` directly and advertises a 1-second edit interval, but it does not yet coalesce bursts of intermediate status updates into a smarter buffer.

Consequence:

- The renderer is simple, testable, and low-risk for the initial Slack adapter milestone.
- Approval flows work with primary/danger button styling and thread-safe message updates.

Remaining caveat:

- Slack-specific rich layouts are still conservative.
- If Slack becomes a primary surface, the next upgrade should add richer per-event thread rendering and more deliberate edit throttling/coalescing.

## 17. Cross-platform approvals and Discord adapter

### 17.1 The unified approval layer still targets inline-button platforms first

Current state:

- `moa-gateway/src/approval.rs` is now the single source of truth for approval callback encoding and default approval buttons.
- Telegram, Slack, and Discord all consume that same callback format and button set.
- The fallback path degrades to text commands when inline buttons are unavailable.

Consequence:

- Approval rendering is now consistent across all current messaging adapters.
- Platform-specific callback parsing no longer drifts between Telegram, Slack, and Discord.

Remaining caveat:

- The generic gateway surface still has no first-class modal representation.
- `PlatformCapabilities.supports_modals` is informative today, but the unified approval flow still chooses inline buttons when available and text fallback otherwise.

### 17.2 Discord thread mapping is anchored to an inbound message

Current state:

- `moa-gateway/src/discord.rs` auto-creates a thread the first time the adapter responds to a guild message that is not already in a thread.
- Direct messages stay in the DM channel.
- Existing Discord threads are reused when the inbound message already arrived inside a thread.

Consequence:

- The documented “one MOA session per Discord thread” model now works for the normal inbound-driven flow.
- Post-decision edits and follow-up tool/status updates land in the same thread.

Remaining caveat:

- Like Telegram and Slack, the Discord adapter still relies on `reply_to` as its routing anchor.
- It cannot proactively open a new conversation without an inbound message or prior synthetic gateway message id.

## 18. Temporal orchestrator

### 18.1 Temporal approval resume had a real wait-condition bug

Current state:

- `moa-orchestrator/src/temporal.rs` now gates the workflow loop differently while paused for approval.
- When `waiting_for_approval` is true, the workflow waits only for an approval decision or cancel request.
- The manual Temporal dev-server integration test in `moa-orchestrator/tests/temporal_orchestrator.rs` now covers the full approval path end to end.

Consequence:

- `ApprovalRequested` no longer deadlocks the workflow.
- An `ApprovalDecided` signal now wakes the workflow, appends the decision event, resumes the turn, executes the approved tool, and reaches completion.

Remaining caveat:

- This was a real correctness bug in the initial Step 17 implementation, not just a missing test.
- The Temporal path should keep at least one ignored live dev-server integration test because replay-only or unit tests would not have caught this specific bug.

### 18.2 Temporal child workflows are still not true Temporal child workflows

Current state:

- `TemporalOrchestrator::spawn_child_workflow()` currently delegates to `start_session()`.
- That means it creates another top-level session workflow with normal session metadata and workflow id allocation.

Consequence:

- Sub-brain work can still be started as an independent Temporal-backed session.
- The public API surface is usable for now without blocking later cloud work.

Remaining caveat:

- This is not yet using Temporal's actual child-workflow semantics from the spec.
- Parent/child cancellation propagation, parent-close behavior, and Temporal-native child observability are not implemented yet.

### 18.3 Worker lifetime is process-scoped and not gracefully stoppable yet

Current state:

- `TemporalRuntime::connect()` starts a dedicated OS thread that owns a current-thread Tokio runtime and runs the Temporal worker.
- The `JoinHandle` is retained only to keep the thread alive for the life of the orchestrator.

Consequence:

- The worker can poll workflows and activities correctly without violating the SDK's non-`Send` constraints.
- The in-process cloud-mode prototype is workable for tests and local development.

Remaining caveat:

- Dropping `TemporalOrchestrator` does not perform a graceful worker shutdown; it effectively detaches the worker thread until process exit.
- A production-ready Temporal deployment likely needs an explicit worker lifecycle manager instead of the current fire-and-forget thread model.

## 19. Cloud hands and MCP

### 19.1 E2B command execution is coupled to the current sandbox RPC surface

Current state:

- `moa-hands/src/e2b.rs` provisions, pauses, resumes, destroys, and reconnects sandboxes using the documented E2B REST lifecycle endpoints.
- Command execution is then routed through the current sandbox RPC/process surface exposed by the sandbox domain, matching the behavior of the published E2B JS SDK as closely as practical.
- Live validation showed that command execution is not a plain JSON POST. It is a Connect JSON stream:
  - `Content-Type: application/connect+json`
  - `Connect-Protocol-Version: 1`
  - request body encoded as a Connect envelope
  - response body decoded as Connect envelopes with `start`, `data`, `end`, and end-stream frames
- Live validation also showed that `POST /sandboxes/{id}/pause` requires a JSON body, even when it is just `{}`.

Consequence:

- The current Step 18 E2B hand provider works for mocked tests and real E2B Cloud runs.
- MOA can treat E2B as a real `HandProvider` without inventing a second execution abstraction just for microVMs.

Remaining caveat:

- The E2B command execution path is less stable than the local or Daytona hands because it depends on an upstream transport shape that is not as cleanly documented as the lifecycle REST API.
- If E2B changes the sandbox RPC surface, `moa-hands/src/e2b.rs` is the first place likely to need adjustment.

### 19.2 MCP remote auth currently applies only to HTTP/SSE transports

Current state:

- `moa-security/src/mcp_proxy.rs` issues session-scoped opaque tokens and injects real credentials only when a remote MCP call is dispatched.
- `moa-hands/src/router.rs` uses that proxy when executing MCP tools discovered from configured servers.
- `moa-hands/src/mcp.rs` supports stdio and remote JSON-RPC transports, with SSE response parsing for remote endpoints that reply as event streams.

Consequence:

- Remote MCP servers can receive credentials without exposing them to the brain or to serialized tool arguments.
- Session-scoped auth is enforced at the router/proxy seam instead of being embedded in prompt-visible tool definitions.

Remaining caveat:

- Stdio MCP auth is still process/env based; the proxy does not inject credentials into subprocess transports.
- The current credential flow is therefore strongest for HTTP/SSE MCP servers and weaker for stdio servers that need secrets at startup.

### 19.3 MCP tool results are still flattened into shell-shaped output

Current state:

- `moa-hands/src/mcp.rs` flattens MCP `content` arrays into the existing `ToolOutput` shape.
- Text-like results are concatenated into `stdout`; MCP `isError` is mapped onto `stderr` and a non-zero exit code.

Consequence:

- Existing brain, approval, and UI code can consume MCP tool results without a larger cross-crate type migration.
- Step 18 landed with low blast radius outside `moa-hands` and `moa-security`.

Remaining caveat:

- Structured MCP results such as rich JSON objects, binary assets, or multiple typed content blocks lose fidelity when reduced to `ToolOutput`.
- A future first-class structured tool result type would make MCP integrations cleaner across the brain, orchestrator, and UI layers.

### 19.4 Daytona live API behavior diverges from the obvious reading of the docs

Current state:

- `moa-hands/src/daytona.rs` now provisions sandboxes successfully against the real Daytona Cloud API.
- The provider uses the live proxy routes that actually answered during end-to-end validation:
  - `.../toolbox/{sandboxId}/process/execute`
  - `.../toolbox/{sandboxId}/files/download`
  - `.../toolbox/{sandboxId}/files/upload`
  - `.../toolbox/{sandboxId}/files/search`
- File upload uses multipart form data, matching the live toolbox behavior.
- Destroy now treats `409 state change in progress` as a retryable transition instead of a hard failure.

Consequence:

- The Daytona provider now passes both mocked tests and real ignored live integration tests.
- Router-level lazy provisioning, hand reuse, pause/resume, and session isolation all work against the real Daytona service.

Remaining caveat:

- The live API rejected explicit sandbox resource fields on the current create path with `Cannot specify Sandbox resources when using a snapshot`, so `moa-hands/src/daytona.rs` currently relies on Daytona's default sandbox class instead of honoring requested CPU/memory overrides.
- The published docs mix `/toolbox/{sandboxId}/toolbox/...` reference paths with curl examples that omit the second `/toolbox`; the real proxy accepted the shorter form during validation.

### 20.1 Local encrypted vault currently uses a generated local passphrase file

Current state:

- `moa-security/src/vault.rs` stores credentials in `vault.enc` as an age-encrypted JSON document.
- The local passphrase is generated automatically on first use and stored beside the vault as `vault.key` with `0600` permissions on Unix.
- This keeps credentials encrypted at rest and gives MOA a concrete local `CredentialVault` implementation without requiring user setup before the first run.

Consequence:

- Step 19 now has a working encrypted local vault with async `get/set/delete/list` operations and test coverage.
- Existing local components can move off env-only credential handling when the runtime is wired to use `FileVault`.

Remaining caveat:

- This is stronger than plaintext-at-rest, but weaker than OS keychain or external secret-manager storage because the decryption material still lives on the same machine.
- If we later want stricter local secret handling, the swap point is the `CredentialVault` trait, not the rest of the security pipeline.

### 20.2 Local Docker hardening currently disables container network access entirely

Current state:

- `moa-hands/src/local.rs` now starts Docker sandboxes with:
  - read-only root filesystem
  - tmpfs scratch mounts
  - `cap-drop=ALL`
  - `no-new-privileges:true`
  - `pids-limit=256`
  - Docker seccomp active (daemon default unless `MOA_DOCKER_SECCOMP_PROFILE` is set)
- The implementation uses `--network none` as the concrete way to block the cloud metadata endpoint from local Docker sandboxes.

Consequence:

- The new Docker hardening integration test can verify `Seccomp: 2`, `NoNewPrivs: 1`, a read-only root mount, and that `169.254.169.254` is unreachable.
- The local hand now has a real hardened container posture instead of a soft long-lived shell.

Remaining caveat:

- This is stricter than the original spec in one dimension: local Docker sandboxes are offline, not just metadata-blocked.
- If we later need outbound network for local containerized tools, we will need a narrower metadata-blocking mechanism than `--network none`.

### 20.3 Tool-result trust boundaries are explicit, but repeated malicious tool loops are still model-driven

Current state:

- `moa-brain/src/harness.rs` now injects a per-turn canary into tool-enabled requests.
- Tool invocations are blocked if they leak either the exact active canary or any `moa_canary_*` marker.
- Tool outputs are always wrapped in `<untrusted_tool_output>...</untrusted_tool_output>` plus an explicit instruction not to follow embedded instructions.
- Suspicious tool output produces `Warning` events instead of being silently fed back into history.

Consequence:

- The instruction hierarchy is now materially stronger: tool results re-enter Stage 6 as low-authority, explicitly untrusted content.
- Step 19 regression tests now cover both canary leakage and malicious tool-output containment.

Remaining caveat:

- If a model keeps emitting fresh malicious tool calls after seeing the resulting `ToolError`/`Warning`, the retry behavior is still governed by the surrounding turn loop rather than a dedicated security circuit breaker.
- If that becomes a real failure mode, the next seam to tighten is the orchestrator/harness retry policy, not the classifier itself.

### 20.4 OpenAI/OpenRouter now use the Responses API, but MOA tool schemas are still translated provider-side

Current state:

- `moa-providers/src/openai.rs` and `moa-providers/src/openrouter.rs` both call the OpenAI-compatible `/responses` API.
- MOA still stores tool schemas in the existing internal format used across the rest of the repo.
- `moa-providers/src/common.rs` translates those schemas into Responses function tools at request time.
- The translation currently sends `strict: false` because the current MOA schemas include optional properties that are not yet normalized into OpenAI's stricter function-schema shape.

Consequence:

- The default local runtime can now use `openai / gpt-5.4` successfully, including streaming and tool use.
- OpenRouter rides the same Responses-compatible translation layer instead of a separate request shape.

Remaining caveat:

- The provider layer is still compensating for schema mismatches that really belong in the shared tool-definition surface.
- If we later want fully strict OpenAI function schemas, the right fix is to normalize tool schemas once in the registry/core model, not to keep adding provider-specific exceptions.

### 20.5 OpenAI metadata forwarding is intentionally lossy

Current state:

- `moa-providers/src/common.rs` forwards request metadata to the Responses API only when each value fits within OpenAI's metadata size limits.
- Oversized internal metadata values such as serialized `tool_schemas` are now dropped before the request is sent.

Consequence:

- The live `moa exec` path now works with the default OpenAI provider instead of failing with `metadata.* string too long`.
- Provider requests still preserve small diagnostic metadata values when they are useful.

Remaining caveat:

- OpenAI/OpenRouter requests no longer carry the full MOA metadata bag verbatim.
- If any downstream debugging or analytics later depends on large metadata fields, those fields will need a different transport than provider metadata.

### 20.6 Capability coverage is precise for default models and best-effort elsewhere

Current state:

- `moa-providers/src/openai.rs` has explicit capabilities and pricing for the supported GPT-5 family used by MOA defaults.
- `moa-providers/src/openrouter.rs` reuses those mappings for OpenAI-family models and has explicit fallbacks for the currently supported Anthropic families routed through OpenRouter.

Consequence:

- `gpt-5.4` is now the repo default model and reports concrete capabilities.
- Known OpenRouter model families report useful capability data instead of a generic placeholder.

Remaining caveat:

- Coverage is still selective, not exhaustive across every OpenAI/OpenRouter model id.
- Adding new default models should include an explicit capability/pricing update rather than relying on the generic fallback.

### 21.1 The Step 21 TUI memory/settings surfaces are functional, but still intentionally shallow

Current state:

- `moa-tui/src/app.rs` now exposes the documented Step 21 shells for sidebar, memory browser, settings, help, command palette, slash completion, and `@file` completion.
- The memory browser in `moa-tui/src/views/memory.rs` supports page browsing, FTS-backed search, wikilink following, and back/forward history.
- The settings panel in `moa-tui/src/views/settings.rs` persists a focused subset of config values and hot-reloads the provider/model path through the existing runtime seam.

Consequence:

- The TUI now has a real feature-complete shell instead of only chat/session/diff overlays.
- Manual PTY smoke tests confirmed that the command palette, memory browser, and settings overlays all open in the live alternate-screen loop.

Remaining caveat:

- The memory browser does not yet implement destructive delete or external-editor open; those actions currently surface explicit status messages instead of performing the operation.
- Markdown rendering in the memory pane is still lightweight text rendering, not full `pulldown-cmark` rich formatting.
- The settings panel intentionally edits a small high-value subset of config rather than every field in `MoaConfig`.

### 21.2 Prompt completion is intentionally simple

Current state:

- Slash completion and `@file` completion now render above the prompt and accept via `Tab`.
- File completion is ranked by a small in-memory frecency map plus path order.
- Sandbox files are scanned from the configured local sandbox root and refreshed on periodic sidebar refreshes.

Consequence:

- The documented completion flows now exist and are visible in the real TUI.
- The implementation stayed local to `moa-tui` without introducing another completion engine or cursor-aware editor abstraction.

Remaining caveat:

- Completion is prompt-text based, not true cursor-position aware editing inside arbitrary multiline input.
- `@file` completion currently only rewrites the trailing token, so paths with embedded spaces are not handled yet.
- File-frecency is process-local and is not yet persisted across TUI restarts.

### 22.1 The local daemon currently owns one mutable runtime, not one runtime per client

Current state:

- `moa-cli/src/daemon.rs` runs a single `ChatRuntime` behind a Unix socket and exposes control-plane commands plus per-session observation streams.
- TUI clients can attach to persisted sessions through that daemon, and sessions continue running after the TUI exits.
- `status` and `sessions` intentionally read persisted state directly from the local session store instead of forcing everything through daemon RPC.

Consequence:

- The Step 22 daemon flow now works end to end: `moa daemon start`, `moa status`, `moa sessions`, `moa resume`, and `moa daemon stop`.
- Session persistence is durable even when no TUI is attached.

Remaining caveat:

- Workspace/model changes still mutate shared daemon runtime state, so they are global to the daemon process rather than scoped per connected client.
- If local multi-client control becomes important, the daemon should promote those settings into per-session state instead of one shared mutable runtime.
- The daemon currently starts with one default active session because `ChatRuntime` always owns a current session.
- If we want a truly idle daemon with zero sessions until first use, the runtime boundary should support lazy session creation instead of eager bootstrap.

### 22.2 Default terminal logging is intentionally quiet so it does not corrupt the TUI

Current state:

- `moa-core/src/telemetry.rs` now initializes the human-readable tracing layer at `WARN` by default while still wiring OTLP export when configured.
- This suppresses noisy info-level runtime logs from libraries such as the cron scheduler when launching the alternate-screen TUI.

Consequence:

- Interactive `moa`, `moa resume`, and `moa attach` no longer print routine runtime logs into the terminal before the TUI renders.
- The TUI and CLI subcommands remain usable without log noise while observability stays available through OTLP.

Remaining caveat:

- Rich local debug logging now requires an explicit code or config change instead of appearing by default on stderr.
- If we want operator-friendly local debug mode later, it should be an explicit CLI/config switch rather than the default interactive behavior.

### 23.1 Cloud deploy currently runs the local runtime shape with cloud-backed storage

Current state:

- `moa-cli/src/daemon.rs` now exposes a cloud-friendly daemon entrypoint with an HTTP `/health` endpoint and graceful shutdown handling.
- `Dockerfile`, `fly.toml`, and `.github/workflows/deploy.yml` now build and launch `moa` with the `cloud` feature set and Fly health-check wiring.
- `moa-session/src/turso.rs` now supports libSQL embedded replicas when `cloud.turso_url` is configured, so local disk remains readable while remote sync is enabled.

Consequence:

- The Step 23 deployment path is now viable for Fly Machines using the existing MOA daemon/runtime.
- Session persistence can be cloud-backed without changing the higher-level local runtime API.

Remaining caveat:

- This is still the local daemon/runtime shape running in a cloud container, not a dedicated Temporal worker/service topology.
- A Fly deployment today is effectively "local orchestrator plus cloud-backed session storage" rather than the final independently scaled cloud-brain architecture described in the docs.
- If we want true cloud-native scaling semantics, the daemon entrypoint should eventually bifurcate into explicit local and cloud worker roles instead of reusing one runtime path for both.

### 23.2 Memory "sync" is currently a configurable shared path, not a distributed file sync layer

Current state:

- `moa-memory/src/lib.rs` now prefers `cloud.memory_dir` when cloud mode is enabled.
- `moa init` and the deployment assets create/use that cloud memory root when configured.

Consequence:

- A cloud deployment can keep wiki memory on a mounted volume or another shared filesystem path instead of always writing to the local default.
- The step is unblocked for single-machine or shared-volume Fly deployments.

Remaining caveat:

- There is still no true bidirectional memory sync protocol, object-store replication layer, or Turso-backed file abstraction for wiki content.
- `moa sync enable` currently upgrades the session/event store into Turso sync, but memory remains dependent on whatever filesystem/storage layer the deployment provides.
- If multi-machine memory convergence matters, memory sync still needs a real replication design instead of a path switch.

### 23.3 `moa sync enable` validates the session-store path, not the full cloud stack

Current state:

- `moa-cli/src/main.rs` now exposes `moa sync enable`.
- The command writes cloud sync config only after it can open the embedded replica and perform an initial database sync.

Consequence:

- Local installs can be migrated into Turso-backed session sync without hand-editing config.
- Misconfiguration around the session database path, Turso URL, or auth token is caught before the config is persisted.

Remaining caveat:

- The command only validates the session-store sync path; it does not validate Fly deployment readiness, memory storage readiness, or platform gateway credentials.
- On this machine, Temporal/cloud-feature builds require `PROTOC=/opt/homebrew/bin/protoc` because the earlier `~/.local/bin/protoc` in `PATH` is not executable.

### 23.4 Fly deployment requires explicit cloud-hand and provider boot configuration

Current state:

- `Dockerfile` now installs `libprotobuf-dev` in addition to `protobuf-compiler`, which is required for the `cloud` build because `prost-wkt-types` needs the standard Google protobuf include files.
- `Dockerfile` and `fly.toml` now set `MOA__CLOUD__HANDS__DEFAULT_PROVIDER=local`, so the cloud image can boot without the optional `daytona` or `e2b` features enabled.
- A live Fly deployment to `moa-brains.fly.dev` succeeded after staging `OPENAI_API_KEY`, `MOA__CLOUD__TURSO_URL`, and `TURSO_AUTH_TOKEN` as Fly secrets.

Consequence:

- The packaged Fly image now builds and starts in the same configuration that passed local Docker validation.
- The deployed app serves `GET /health` successfully and runs with the mounted Fly volume plus Turso-backed session sync configuration.

Remaining caveat:

- The daemon still constructs the default LLM provider during boot, so health-only startup currently requires a valid provider API key secret even before the first user request.
- In live validation, a manually stopped machine took roughly 6 seconds to answer the next `/health` request, while a suspended machine resumed in about 1.29 seconds. That is close to, but not consistently below, the sub-second resume target from the step spec.

### 24.1 Scoped `MemoryStore` page operations close the earlier trait gap

Current state:

- `MemoryStore::read_page`, `write_page`, and `delete_page` now all take `MemoryScope`.
- `MemorySearchResult` now carries its originating scope so callers can safely follow a search hit with an explicit page read.
- `memory_write` now supports create-or-update behavior instead of only updating pre-existing uniquely resolved pages.

Remaining note:

- `FileMemoryStore::{read,write,delete}_page_in_scope` still exist as compatibility shims for concrete callers, but the trait surface is now the primary API and new code should prefer it directly.

### 25.1 Context stages now own their async I/O directly

Current state:

- `ContextProcessor::process()` is now async across `moa-core` and `moa-brain`.
- Stage 4 (`SkillInjector`), Stage 5 (`MemoryRetriever`), and Stage 6 (`HistoryCompiler`) now receive their dependencies through constructor injection and do their own I/O inside `process()`.
- The pipeline runner no longer preloads stage inputs into `WorkingContext.metadata` via stringly-typed JSON keys before invoking those stages.

Consequence:

- The pipeline runner is simpler and stage logic is no longer split between `pipeline/mod.rs` and stage-local formatting code.
- The earlier metadata preload indirection for skills, memory, and history is gone, so the main async brain path is easier to follow and less fragile under refactors.

Remaining note:

- `WorkingContext.metadata` still exists for legitimate shared request state such as tool schemas and future cross-stage metrics.
- If that map starts accumulating new stage-specific payload contracts again, the repo will drift back toward the same coupling problem that this step removed.

### 26.1 `ToolOutput` now preserves structure, but the model still receives flattened text

Current state:

- `ToolOutput` is now a content-block type with `Vec<ToolContent>`, `is_error`, optional `structured` data, and `duration`.
- Process-backed tools preserve shell metadata by storing `stdout`, `stderr`, and `exit_code` in `structured`, while built-in tools such as `memory_search` can return both a human-readable summary and structured JSON results.
- `Event::ToolResult` now stores the richer `ToolOutput` value directly instead of flattening it into a string.

Consequence:

- MCP and built-in tools can retain structured results without losing fidelity at the event-log boundary.
- TUI and gateway renderers can later distinguish plain text from structured JSON without re-parsing shell-shaped strings.

Remaining note:

- The LLM-facing path still uses `ToolOutput::to_text()` in the brain harness and history compiler, so the model sees a flattened text rendering rather than typed content blocks.
- That is the correct short-term tradeoff, but if the repo later wants model-visible structured tool results, the provider request layer will need a first-class tool-result representation instead of a text-only flattening step.

### 27.1 Tool metadata now lives in `moa-core`, while routing stays in `moa-hands`

Current state:

- `BuiltInTool`, `ToolContext`, `ToolDefinition`, `ToolPolicySpec`, `ToolInputShape`, and `ToolDiffStrategy` now live in `moa-core`.
- `ToolRegistry`, `ToolRouter`, and `ToolExecution` remain in `moa-hands`, where the actual routing and execution logic belongs.
- `moa-hands` re-exports the moved interface types so existing import paths continue to work during the transition.

Consequence:

- Crates such as `moa-brain` and `moa-security` no longer need to depend on `moa-hands` just to talk about tool metadata.
- The dependency direction is cleaner: core defines what a tool is, and hands defines how tools are executed.

Remaining note:

- `ToolRegistry` still stores execution state separately from the shared `ToolDefinition`, so there are now two closely related internal shapes in `moa-hands`.
- That is intentional for layering, but if registry metadata starts drifting from the core definition again, the next cleanup step should be to tighten construction helpers around the registry entry type rather than moving routing concerns back into `moa-core`.

### 28.1 Docker-backed local hands now route file tools through `docker exec`

Current state:

- `LocalHandProvider` now routes `file_read`, `file_write`, and `file_search` through `docker exec` when the active hand is Docker-backed.
- The container workspace mount is recorded at provisioning time and reused for both `bash` and file tools, so all hand-routed filesystem operations target the same in-container path.
- Host-path file access remains the behavior for pure local hands.

Consequence:

- Docker-backed hands are no longer only partially containerized: shell commands and file tools now run through the same isolation boundary.
- The ignored Docker roundtrip test now checks that `file_write`, `file_read`, `file_search`, and `bash cat` all observe the same container filesystem.

Remaining note:

- The current fallback behavior still drops back to host-path file access when `docker exec` fails due to Docker transport issues such as a lost container or daemon connectivity failure.
- That matches the local provider's existing graceful-degradation posture, but it also means a broken Docker hand can temporarily lose the strict isolation guarantee instead of hard-failing immediately.

### 29.1 Approval replay now comes from durable `ApprovalPrompt` data

Current state:

- `Event::ApprovalRequested` now stores the full `ApprovalPrompt` alongside the top-level `request_id`, `tool_name`, `input_summary`, and `risk_level` fields.
- The orchestrator and buffered brain harness both persist the same rich prompt data that the runtime sends to the TUI, including parsed parameters, allow patterns, and file diffs.
- TUI session rehydration now prefers the persisted prompt directly and only falls back to a minimal placeholder card when replaying older logs that predate this field.

Consequence:

- Switching tabs, resuming from disk, and future non-TUI clients can reconstruct the same approval card fidelity that a live runtime session sees.
- Approval diffs and parameter summaries are no longer runtime-only state that disappears once the broadcast channel is gone.

Remaining note:

- Old session logs still deserialize because `prompt` is optional, but those historical approvals necessarily rehydrate as the older minimal card shape with no diffs or parsed parameters.
- The current local SQLite event log stores file diffs inline; if approval payloads become very large in cloud mode, the next optimization should be a claim-check pattern instead of embedding arbitrarily large diffs in every event row.

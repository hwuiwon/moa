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

## Tool output and schema representation

Tool results are structured internally but flattened for the model, and provider-facing schemas are translated at request time. The shared root cause is the absence of a first-class structured tool-result type in the core model and a normalized schema shape in the tool registry.

### MCP tool results are flattened into shell-shaped output

- `moa-hands/src/mcp.rs` flattens MCP `content` arrays into the existing `ToolOutput` shape. Text-like results are concatenated into `stdout`; MCP `isError` is mapped onto `stderr` and a non-zero exit code.
- Existing brain, approval, and UI code can consume MCP tool results without a larger cross-crate type migration.
- Structured MCP results such as rich JSON objects, binary assets, or multiple typed content blocks lose fidelity when reduced to `ToolOutput`. A future first-class structured tool result type would make MCP integrations cleaner across the brain, orchestrator, and UI layers.

### `ToolOutput` preserves structure, but the model still receives flattened text

- `ToolOutput` is now a content-block type with `Vec<ToolContent>`, `is_error`, optional `structured` data, and `duration`.
- Process-backed tools preserve shell metadata by storing `stdout`, `stderr`, and `exit_code` in `structured`, while built-in tools such as `memory_search` can return both a human-readable summary and structured JSON results.
- `Event::ToolResult` stores the richer `ToolOutput` value directly instead of flattening it into a string.
- The LLM-facing path still uses `ToolOutput::to_text()` in the brain harness and history compiler, so the model sees a flattened text rendering rather than typed content blocks. If the repo later wants model-visible structured tool results, the provider request layer will need a first-class tool-result representation.

### OpenAI/OpenRouter tool schemas are translated provider-side

- `moa-providers/src/openai.rs` and `moa-providers/src/openrouter.rs` both call the OpenAI-compatible `/responses` API.
- `moa-providers/src/common.rs` translates MOA's internal tool schemas into Responses function tools at request time, currently sending `strict: false` because MOA schemas include optional properties not yet normalized into OpenAI's stricter function-schema shape.
- The provider layer is compensating for schema mismatches that really belong in the shared tool-definition surface. The right fix is to normalize tool schemas once in the registry/core model.

### OpenAI metadata forwarding is intentionally lossy

- `moa-providers/src/common.rs` forwards request metadata to the Responses API only when each value fits within OpenAI's metadata size limits. Oversized values such as serialized `tool_schemas` are dropped before the request is sent.
- The live `moa exec` path works with the default OpenAI provider instead of failing with `metadata.* string too long`.
- If any downstream debugging or analytics later depends on large metadata fields, those fields will need a different transport than provider metadata.

### Capability coverage is precise for default models, best-effort elsewhere

- `moa-providers/src/openai.rs` has explicit capabilities and pricing for the supported GPT-5 family. `moa-providers/src/openrouter.rs` reuses those mappings for OpenAI-family models and has explicit fallbacks for the currently supported Anthropic families.
- Coverage is selective, not exhaustive across every model id. Adding new default models should include an explicit capability/pricing update rather than relying on the generic fallback.

---

## Memory maintenance and sync

Memory consolidation, branching, and cross-machine sync are all implemented at the store level but not yet fully wired into the live runtime or distributed across machines.

### Consolidation is heuristic, not yet LLM-driven

- `moa-memory/src/consolidation.rs` performs deterministic maintenance locally: relative date normalization, port-claim contradiction resolution, pruning entities with `metadata.entity_exists = false`, confidence decay for old unreferenced pages, orphan detection, `MEMORY.md` regeneration and `_log.md` append.
- The behavior is stable and cheap, appropriate for local hourly maintenance. It is implemented and testable without network or provider availability.
- Consolidation currently understands only a narrow set of contradiction patterns and stale-page signals — it matches the architectural role but not the full eventual intelligence implied by the spec's LLM-maintainer sketch.

### Branch writes exist but the runtime still writes directly to mainline memory

- `moa-memory/src/branching.rs` provides branch-local writes under `.branches/`, a JSON change manifest, and deterministic reconciliation back into the main scope.
- The live tool/runtime path still uses direct `write_page_in_scope()` / `memory_write`, not branch-local writes.
- Concurrent-write isolation is implemented at the store level and covered by tests, but the branch/reconcile model is not yet wired into the orchestrator or tools as the default write path.
- If true concurrent cloud writers become a near-term requirement, the router/runtime should switch to branched writes instead of direct mainline writes.

### Scheduled consolidation runs for workspace scopes only

- `LocalOrchestrator` registers an hourly maintenance job that calls `FileMemoryStore::run_due_consolidations(...)`. That scheduler currently derives scopes from session history and only executes workspace consolidations.
- User-scope consolidation is not scheduled yet. This is intentional while local user memory still shares one physical `memory/` root regardless of user id, but it means personal memory does not yet get automatic dream-cycle maintenance.

### Memory "sync" is a configurable shared path, not a distributed sync layer

- `moa-memory/src/lib.rs` prefers `cloud.memory_dir` when cloud mode is enabled. A cloud deployment can keep wiki memory on a mounted volume or another shared filesystem path.
- There is still no true bidirectional memory sync protocol, object-store replication layer, or Turso-backed file abstraction for wiki content.
- `moa sync enable` upgrades the session/event store into Turso sync, but memory remains dependent on whatever filesystem/storage layer the deployment provides.

---

## Cloud sandbox provider fragility

Both E2B and Daytona integrations depend on upstream API surfaces that diverge from published documentation. These are the most likely crates to need adjustment when upstream changes.

### E2B command execution is coupled to the sandbox RPC surface

- `moa-hands/src/e2b.rs` provisions, pauses, resumes, destroys, and reconnects sandboxes using the documented E2B REST lifecycle endpoints.
- Command execution uses a Connect JSON stream (`application/connect+json`, `Connect-Protocol-Version: 1`) with envelope encoding — not a plain JSON POST. Live validation also showed that `POST /sandboxes/{id}/pause` requires a JSON body, even when it is just `{}`.
- The E2B command execution path is less stable than the local or Daytona hands because it depends on an upstream transport shape that is not as cleanly documented as the lifecycle REST API. If E2B changes the sandbox RPC surface, `moa-hands/src/e2b.rs` is the first place likely to need adjustment.

### Daytona live API behavior diverges from the docs

- `moa-hands/src/daytona.rs` provisions sandboxes successfully against the real Daytona Cloud API using the live proxy routes that actually answered during end-to-end validation (`.../toolbox/{sandboxId}/process/execute`, `.../files/download`, `.../files/upload`, `.../files/search`).
- File upload uses multipart form data. Destroy treats `409 state change in progress` as a retryable transition instead of a hard failure.
- The live API rejected explicit sandbox resource fields with `Cannot specify Sandbox resources when using a snapshot`, so the provider currently relies on Daytona's default sandbox class instead of honoring requested CPU/memory overrides.
- The published docs mix `/toolbox/{sandboxId}/toolbox/...` reference paths with curl examples that omit the second `/toolbox`; the real proxy accepted the shorter form.

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

## Single-process runtime boundaries

Several caveats share a root cause: the current runtime assumes a single local process with in-memory broadcast channels. Remote observers, multi-client daemons, and quiet logging are all consequences of that boundary.

### Turn execution has one streamed source of truth

- The shared streamed turn engine lives in `moa-brain/src/harness.rs`, with lower-level stream helpers in `moa-brain/src/turn.rs`. `run_brain_turn_with_tools()` is a buffered wrapper around that engine.
- `LocalOrchestrator` calls the same streamed engine for live sessions. `BrainOrchestrator` exposes `observe_runtime()`, and the local TUI is only a stream consumer. `moa exec` and the local TUI both observe the same runtime event stream.
- `TemporalOrchestrator::observe_runtime()` still returns `None`; cloud/runtime observation needs an explicit transport such as SSE or WebSocket. Remote observers need a bridging layer if they cannot subscribe to an in-process broadcast channel.

### The local daemon owns one mutable runtime, not one per client

- `moa-cli/src/daemon.rs` runs a single `ChatRuntime` behind a Unix socket and exposes control-plane commands plus per-session observation streams.
- TUI clients can attach to persisted sessions through that daemon, and sessions continue running after the TUI exits.
- Workspace/model changes mutate shared daemon runtime state, so they are global to the daemon process rather than scoped per connected client.
- The daemon starts with one default active session because `ChatRuntime` always owns a current session. A truly idle daemon with zero sessions until first use would require lazy session creation.

### Default terminal logging is quiet to avoid corrupting the TUI

- `moa-core/src/telemetry.rs` initializes the human-readable tracing layer at `WARN` by default while still wiring OTLP export when configured.
- Interactive `moa`, `moa resume`, and `moa attach` no longer print routine runtime logs into the terminal before the TUI renders.
- Rich local debug logging now requires an explicit code or config change. If we want operator-friendly local debug mode later, it should be an explicit CLI/config switch.

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

---

## Backward compatibility and graceful degradation

These caveats share a pattern: the new code is correct, but older data or fallback paths degrade gracefully rather than failing hard.

### Scoped `MemoryStore` page operations close the earlier trait gap

- `MemoryStore::read_page`, `write_page`, and `delete_page` all take `MemoryScope`. `MemorySearchResult` carries its originating scope so callers can safely follow a search hit with an explicit page read. `memory_write` supports create-or-update behavior.
- `FileMemoryStore::{read,write,delete}_page_in_scope` still exist as compatibility shims for concrete callers, but the trait surface is now the primary API and new code should prefer it directly.

### Docker-backed local hands route file tools through `docker exec`

- `LocalHandProvider` routes `file_read`, `file_write`, and `file_search` through `docker exec` when the active hand is Docker-backed. The container workspace mount is recorded at provisioning time and reused for both `bash` and file tools.
- Docker-backed hands are no longer only partially containerized: shell commands and file tools run through the same isolation boundary.
- The current fallback behavior drops back to host-path file access when `docker exec` fails due to Docker transport issues. This matches the existing graceful-degradation posture, but a broken Docker hand can temporarily lose the strict isolation guarantee instead of hard-failing.

### Approval replay comes from durable `ApprovalPrompt` data

- `Event::ApprovalRequested` stores the full `ApprovalPrompt` alongside the top-level `request_id`, `tool_name`, `input_summary`, and `risk_level` fields.
- The orchestrator and buffered brain harness both persist the same rich prompt data that the runtime sends to the TUI, including parsed parameters, allow patterns, and file diffs.
- TUI session rehydration prefers the persisted prompt directly and falls back to a minimal placeholder card when replaying older logs that predate this field.
- Old session logs still deserialize because `prompt` is optional, but historical approvals rehydrate as the older minimal card shape.
- The current local SQLite event log stores file diffs inline; if approval payloads become very large in cloud mode, the next optimization should be a claim-check pattern instead of embedding arbitrarily large diffs in every event row.

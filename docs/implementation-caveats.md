# Implementation Caveats

Implementation notes and design caveats surfaced while building the current MOA workspace. These are not necessarily bugs, but they are places where the current trait surface or helper behavior is awkward enough to review before later steps build on top of them.

Caveats are grouped by root cause / architectural boundary, not by the crate where the symptom first appears. Fixing the root of a group typically unblocks every caveat in it.

---

## Messaging: channel callbacks are not first-class typed events

Messaging adapters normalize channel-specific callback payloads into text control messages instead of typed messaging events. The fix is the same everywhere: widen `ChannelAdapter` to emit structured callback events alongside `InboundMessage`.

### Slack interactive actions are normalized back into control messages

- `crates/moa-messaging/src/slack.rs` receives Block Kit button actions over Socket Mode.
- The core `ChannelAdapter` trait still only emits `InboundMessage`.
- The adapter converts current interactive actions into normalized text commands.
- If adapters need richer structured callbacks later, `InboundMessage.text`
  should stop carrying control commands. Tenant action review decisions and
  builtin async-authz challenge decisions should remain distinct callback types
  rather than sharing a generic control-message path.

---

## Messaging: outbound routing requires an inbound anchor

The Slack adapter resolves outbound destinations from `reply_to` and cannot proactively start conversations. The shared fix is an explicit destination field on `OutboundMessage` or the adapter trait.

### Slack outbound routing depends on an existing reply anchor

- `OutboundMessage` still has no explicit Slack destination.
- `crates/moa-messaging/src/slack.rs` resolves channel/thread targets from `reply_to`, using either a known inbound Slack message timestamp or a previously sent synthetic messaging message id.
- The intended session model works: one MOA session per Slack thread, with replies and edits anchored correctly.
- The adapter cannot proactively open a brand-new channel/thread without a prior inbound anchor.

---

## Messaging: conservative rendering

Slack rendering is intentionally minimal. Upgrading it requires a proper channel-safe formatting layer with escaping and richer markup.

### Slack rendering is intentionally minimal Block Kit

- `crates/moa-messaging/src/renderer.rs` splits Slack output at the 40K text cap and keeps normal text/code/diff output text-first.
- Block Kit remains an optional rendering detail for interactive controls.
- The adapter uses `chat.update` directly and advertises a 1-second edit interval, but does not yet coalesce bursts of intermediate status updates into a smarter buffer.
- If Slack becomes a primary surface, the next upgrade should add richer per-event thread rendering and more deliberate edit throttling/coalescing.

---

## Security posture — intentional trade-offs

These caveats are deliberate security trade-offs where the current implementation is good enough for the current threat model but has a known upgrade path for stricter requirements.

### MCP remote auth applies only to HTTP/SSE transports

- `crates/moa-security/src/mcp_proxy.rs` issues session-scoped opaque tokens and injects real credentials only when a remote MCP call is dispatched. `crates/moa-hands/src/mcp.rs` supports stdio and remote JSON-RPC transports, with SSE response parsing for remote endpoints.
- Remote MCP servers receive credentials without exposing them to the brain or to serialized tool arguments. Session-scoped auth is enforced at the router/proxy seam.
- Stdio MCP auth is still process/env based; the proxy does not inject credentials into subprocess transports. The credential flow is strongest for HTTP/SSE MCP servers and weaker for stdio servers that need secrets at startup.

### Local Docker hardening disables container network access entirely

- `crates/moa-hands/src/local.rs` starts Docker sandboxes with read-only root filesystem, tmpfs scratch mounts, `cap-drop=ALL`, `no-new-privileges:true`, `pids-limit=256`, and Docker seccomp active. The implementation uses `--network none` to block the cloud metadata endpoint.
- This is stricter than the original spec: local Docker sandboxes are fully offline, not just metadata-blocked.
- If we later need outbound network for local containerized tools, we will need a narrower metadata-blocking mechanism than `--network none`.

### Repeated malicious tool loops are still model-driven

- `crates/moa-brain/src/harness.rs` injects a per-turn canary into tool-enabled requests. Tool invocations are blocked if they leak the active canary or any `moa_canary_*` marker. Tool outputs are wrapped in `<untrusted_tool_output>` with an explicit instruction not to follow embedded instructions. Suspicious output produces `Warning` events.
- The instruction hierarchy is materially stronger and regression tests cover both canary leakage and malicious tool-output containment.
- If a model keeps emitting fresh malicious tool calls after seeing the resulting `ToolError`/`Warning`, the retry behavior is still governed by the turn loop rather than a dedicated security circuit breaker. The next seam to tighten is the orchestrator/harness retry policy.

---

## Deployment and boot configuration

These caveats relate to the gap between "cloud build succeeds" and "cloud deployment is fully self-service."

### LLM fact extraction is journal-safe but still rollout-gated

- `moa-memory-ingest` keeps the slow-path Restate step name as `"extract"` and
  only extends `ExtractedFact` with an optional `confidence` field that defaults
  during deserialization. Old journal entries without confidence still replay.
- `memory.extraction.enabled` defaults to `false`. The orchestrator installs
  the LLM extractor only when that flag is enabled and the configured
  `memory.extraction.api_key_env` exists; otherwise it logs that the heuristic
  extractor is active.
- Production rollout order should be: deploy with extraction disabled, verify
  normal ingestion and contradiction behavior, then enable per environment.
- The shared Cohere chat transport now lives inside `moa-memory-ingest` because
  the current chat consumers are ingestion-local. Do not create a
  `moa-providers` chat abstraction for this alone; the trigger is a second
  vendor or a chat consumer outside ingest. The embedder and reranker keep their
  existing clients because their API surfaces differ.

### Entity resolution v2 is scope-local and needs live-geometry monitoring

- `moa-memory-ingest` resolves entities by exact normalized name first, then by
  same-scope vector blocking plus an `EntityMergeVerifier`. The verifier uses
  the shared Cohere chat client for live recording and recorded fixtures for
  hermetic replay.
- Existing Entity nodes without embeddings are tolerated but cannot appear in
  the embedding block. Prompt 09 owns any historical backfill of entity
  embeddings and node-level alias consolidation.
- Merge aliases are currently written to the newly created entity edge because
  `GraphStore` has no node-property update operation. This preserves the signal
  without widening the graph mutation API mid-ingest.
- The PR hermetic lane uses deterministic cached embeddings; real Cohere
  geometry can produce a different candidate set at the 0.80 threshold. Prompt
  08's live lane should report `entity_fragmentation` so the threshold can be
  calibrated against live vectors.

### Cloud handler startup requires explicit database, Restate, and provider configuration

- `Dockerfile` builds `moa-orchestrator-bin` and installs it as `/usr/local/bin/moa-orchestrator`.
- The orchestrator process loads shared `MoaConfig` from flat `MOA_...` environment variables such as `MOA_DATABASE_URL`, `MOA_RESTATE_ADMIN_URL`, `MOA_RESTATE_INGRESS_URL`, `MOA_LOCAL_SANDBOX_DIR`, `MOA_LOCAL_MEMORY_DIR`, `MOA_LOCAL_DOCKER_ENABLED`, observability settings, and metrics settings.
- Provider registration is environment-backed. Health readiness validates Postgres and optional Restate registration, not provider reachability; real LLM requests still need a configured key such as `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, or `GOOGLE_API_KEY`.
- `fly.toml` carries flat `MOA_...` settings. Kubernetes and Fly deployments should use environment variables for runtime configuration.
- In live validation, a manually stopped machine took roughly 6 seconds to answer the next `/health` request, while a suspended machine resumed in about 1.29 seconds — close to, but not consistently below, the sub-second resume target from the step spec.

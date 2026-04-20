# MOA Architecture

This document is the high-level map of MOA. It answers: *what are the moving parts, how do they talk, what guarantees do they hold, and where do I look when something goes wrong?*

Deep dives live in [`docs/`](docs/) — each section below cross-links to the authoritative spec. For **runtime sequence diagrams** (mermaid), see [`sequence-diagrams.md`](sequence-diagrams.md).

---

## 1. Mental model

MOA is built on two ideas:

1. **Many brains, many hands.** Reasoning is separated from execution. A **brain** is a stateless harness that compiles context, calls an LLM, and emits events. A **hand** is an execution environment (Docker container, Daytona workspace, E2B microVM, MCP server) that runs `execute(tool, input) → output`. The brain never knows what kind of hand it's talking to.

2. **The session event log is the source of truth.** Everything durable flows through an append-only Postgres log. Brains are crash-tolerant because they can always wake from the log and resume. Live observation is a *tail* on top of durable history — never a replacement for it.

Everything else in this document is a consequence of those two ideas.

---

## 2. System diagram

```
┌─────────────────────────────────────────────────────────────────┐
│                        USER INTERFACES                          │
│ Telegram │ Slack │ Discord │ Desktop App │ CLI (moa exec) │ ... │
└─────┬─────┴───┬───┴────┬────┴──────┬──────┴─────────┬───────────┘
      │         │        │           │                │
      ▼         ▼        ▼           ▼                ▼
┌─────────────────────────────────────────────────────────────────┐
│                    GATEWAY (moa-gateway)                        │
│  PlatformAdapter per channel  ·  normalizes inbound             │
│  Renders outbound (text / diff / tool card / approval)          │
└───────────────────────────┬─────────────────────────────────────┘
                            │
                            ▼
┌─────────────────────────────────────────────────────────────────┐
│           BRAIN ORCHESTRATOR (moa-orchestrator)                 │
│  Cloud:  Restate-backed runtime on Kubernetes                   │
│  Local:  LocalOrchestrator   — tokio tasks + mpsc/broadcast     │
│                                                                 │
│  · spawn / recover brains from session log                      │
│  · route signals: approvals, stop, queue                        │
│  · supervise brain lifecycle, destroy hands on terminal exit    │
│  · schedule crons: consolidation, skill improvement             │
└────────┬──────────────────┬─────────────────┬───────────────────┘
         │                  │                 │
         ▼                  ▼                 ▼
┌──────────────┐    ┌──────────────┐   ┌──────────────┐
│   BRAIN A    │    │   BRAIN B    │   │   BRAIN C    │
│ (moa-brain)  │    │  stateless   │   │   harness    │
└──┬───┬───┬───┘    └──┬───┬───┬───┘   └──┬───┬───┬───┘
   │   │   │           │   │   │          │   │   │
   ▼   ▼   ▼           ▼   ▼   ▼          ▼   ▼   ▼
┌──────────────────────────────────────────────────────┐
│            HANDS (moa-hands) — pluggable             │
│  Local exec │ Docker │ Daytona │ E2B µVM │ MCP       │
│  HandProvider::execute(tool, input) → ToolOutput     │
└──────────────────────────────────────────────────────┘
         │               │               │
         ▼               ▼               ▼
┌──────────────────────────────────────────────────────┐
│         SESSION EVENT LOG (moa-session, Postgres)    │
│  append-only  ·  emit_event / get_events / wake      │
└──────────────────────────────────────────────────────┘
         │
         ▼
┌──────────────────────────────────────────────────────┐
│            MEMORY (moa-memory) — file-wiki           │
│  User wiki │ Workspace wiki │ FTS index │ Skills     │
│  Consolidation (Dream) · Git-branch writes + merge   │
└──────────────────────────────────────────────────────┘
```

Source: [`docs/01-architecture-overview.md`](docs/01-architecture-overview.md).

---

## 3. Core traits (the interface seam)

All stable interfaces live in [`moa-core`](moa-core/). Implementations swap freely between local and cloud without touching brain logic.

| Trait | Responsibility | Local impl | Cloud impl |
|---|---|---|---|
| `BrainOrchestrator` | Session lifecycle, signals, observation, cron | `LocalOrchestrator` | Restate-backed runtime |
| `SessionStore` | Append-only event log + queryable metadata | `PostgresSessionStore` | `PostgresSessionStore` (managed) |
| `HandProvider` | `provision` / `execute` / `pause` / `resume` / `destroy` | `LocalHandProvider` | `DaytonaHandProvider`, `E2BHandProvider` |
| `LLMProvider` | Streaming completion + capabilities (context window, caching, tools) | Anthropic / OpenAI / Gemini | Same |
| `PlatformAdapter` | Messaging-channel I/O + rendering caps | Desktop/CLI local bridge | Telegram / Slack / Discord |
| `MemoryStore` | Wiki page read/write + search + indexing | `FileMemoryStore` | Same (synced volume) |
| `ContextProcessor` | One stage of the context compilation pipeline | 7 built-in processors | Same |
| `CredentialVault` | Service credential get/set/delete/list | `FileVault` (age-encrypted) | `HashiCorpVault` |

Full trait signatures: [`docs/01-architecture-overview.md`](docs/01-architecture-overview.md) §Core trait hierarchy.

---

## 4. Workspace layout

```
moa/
├── moa-core/          # Types, traits, config, errors — the contract
├── moa-brain/         # Harness loop + 7-stage context pipeline + compaction
├── moa-session/       # PostgresSessionStore, event schema, FTS, replay
├── moa-memory/        # File-wiki, ingest, consolidation, git-branch writes
├── moa-hands/         # Local/Docker/Daytona/E2B/MCP, ToolRouter
├── moa-providers/     # Anthropic, OpenAI, Gemini — streaming + prompt caching
├── moa-orchestrator/  # LocalOrchestrator (tokio) + Restate-backed cloud runtime
├── moa-gateway/       # Telegram / Slack / Discord + platform renderers + approvals
├── moa-security/      # Credential vault, MCP proxy, sandbox policies, injection
├── moa-skills/        # Agent Skills registry, distillation, self-improvement
├── moa-eval/          # Evaluation harness (suites, reporters, CI gating)
├── moa-runtime/       # Shared bootstrap wiring for local + cloud
├── moa-cli/           # `moa` binary (clap-based) + daemon
└── moa-desktop/       # GPUI desktop app — NOT a default-member
```

`moa-desktop` is excluded from default builds because GPUI has heavy native deps. Build it explicitly:

```bash
cargo build -p moa-desktop
```

---

## 5. Runtime modes

The same harness runs in two modes. Only the wiring differs.

### Local mode — `moa exec`, `moa-desktop`, `moa daemon serve`

```
BrainOrchestrator  →  LocalOrchestrator (tokio + mpsc + broadcast)
SessionStore       →  PostgresSessionStore (dockerized Postgres 18)
HandProvider       →  LocalHandProvider (direct exec, or Docker if available)
PlatformAdapter    →  Desktop GPUI window / CLI stdin-stdout
CredentialVault    →  FileVault (age-encrypted ~/.moa/vault.enc)
```

- Zero cloud dependencies.
- Postgres is the only required local service (`make dev`).
- Sessions survive the process restarting — replay from event log.

### Cloud mode — `MOA__CLOUD__ENABLED=true`

```
BrainOrchestrator  →  Restate-backed runtime (objects, services, workflows)
SessionStore       →  PostgresSessionStore (Neon / managed Postgres)
HandProvider       →  DaytonaHandProvider (default) or E2BHandProvider (Tier 2)
PlatformAdapter    →  Telegram / Slack / Discord adapters
CredentialVault    →  HashiCorpVault
```

- Durable execution via Restate — journaled handlers and idempotent writes (`UNIQUE(session_id, sequence_num)`) keep retries safe.
- Fly.io Machines host brains. Auto-suspend on idle (~5 min) → only storage cost when nobody's active. Auto-resume in sub-second when a message arrives.
- Multi-session: orchestrator tracks many concurrent workflows.

Details: [`docs/02-brain-orchestration.md`](docs/02-brain-orchestration.md).

---

## 6. End-to-end request flow

Walking one request all the way through, so the layers connect:

```
1.  User sends "deploy to staging" via Telegram
2.  TelegramAdapter → normalizes → InboundMessage { user_id, workspace_id, text }
3.  Gateway routes → BrainOrchestrator.start_session() or .signal(QueueMessage)
4.  Orchestrator spawns / signals a Brain workflow
5.  Brain.wake(session_id)  →  load events from SessionStore
6.  Brain runs the 7-stage context pipeline:
    1. IdentityProcessor       — static system prompt         ┐
    2. InstructionProcessor    — workspace + user prefs       │  cached
    3. ToolDefinitionProcessor — active tool schemas          │  prefix
    4. SkillInjector           — skill metadata (Tier 1)      ┘
    ─────── cache_breakpoint ───────
    5. MemoryRetriever         — relevant wiki pages
    6. HistoryCompiler         — checkpoint + recent turns
    7. CacheOptimizer          — deterministic ordering, report cache ratio
7.  Brain calls LLMProvider.complete(compiled_context)
8.  LLM responds with tool_call: bash("fly deploy --app staging")
9.  Brain emits ApprovalRequested event → SessionStore
10. Orchestrator routes approval → Gateway → Telegram inline buttons
11. User taps [Allow Once]
12. TelegramAdapter sends ApprovalDecided signal → Orchestrator → Brain
13. Brain provisions a hand via HandProvider.provision(Tier1)
14. Brain calls hand.execute("bash", "fly deploy --app staging")
15. Hand returns ToolOutput → Brain emits ToolResult event
16. LLM says "Deployment complete. Staging is now v2.3.1."
17. Brain emits BrainResponse event
18. Brain considers memory write → writes deploy-skill update if applicable
19. Brain emits SessionCompleted
20. Gateway renders final message to Telegram
```

If anything dies between step 5 and step 20, a new brain can resume from the last event. **No recovery code.** The replay is the recovery.

---

## 7. Session event log ([`moa-session`](moa-session/))

Postgres is the single source of truth. Every change to a session is an event. The schema lives in [`docs/05-session-event-log.md`](docs/05-session-event-log.md).

Key event types:

- **Lifecycle:** `SessionCreated`, `SessionStatusChanged`, `SessionCompleted`
- **User input:** `UserMessage`, `QueuedMessage`
- **Brain output:** `BrainThinking`, `BrainResponse`
- **Tools:** `ToolCall`, `ToolResult`, `ToolError`
- **Approvals:** `ApprovalRequested`, `ApprovalDecided`
- **Memory:** `MemoryRead`, `MemoryWrite`
- **Hands:** `HandProvisioned`, `HandDestroyed`, `HandError`
- **Compaction:** `Checkpoint`
- **Errors:** `Error` (with `recoverable: bool`), `Warning`

### Compaction

Triggered when event count since last checkpoint > 100 **or** estimated history tokens > 70 % of model context. A two-phase process:

1. **Memory flush** — the brain is given only `memory_write` and asked to preserve anything important.
2. **Checkpoint summary** — LLM summarizes events-since-last-checkpoint into a `Checkpoint` event.

Errors are **always** preserved through compaction — they're the strongest signal against repeated mistakes.

### Observation semantics

`BrainOrchestrator::observe()` replays durable history first, then attaches a live broadcast tail if a brain is active. If the tail lags beyond its in-memory buffer, the stream returns an error so the caller can reopen from durable history — silent loss is considered a bug.

---

## 8. Context compilation ([`moa-brain`](moa-brain/))

The single biggest cost/latency lever in a production agent is KV-cache hit rate. Cached tokens cost ~10× less. The pipeline is deliberately ordered to maximize stable prefix reuse.

```
┌─ stable prefix (cached across turns)  ─┐
│  1. IdentityProcessor                  │
│  2. InstructionProcessor               │
│  3. ToolDefinitionProcessor            │
│  4. SkillInjector                      │
├── cache_breakpoint ────────────────────┤
│  5. MemoryRetriever                    │
│  6. HistoryCompiler                    │
│  7. CacheOptimizer                     │
└────────────────────────────────────────┘
```

Guardrails built into the pipeline:

| Failure mode | Check |
|---|---|
| Context Poisoning (malformed tool output) | Validate against expected schema before appending |
| Context Distraction (bloated history) | Trigger compaction past token threshold |
| Context Confusion (too many tools) | Hard cap at 30; warn at 20 |
| Context Clash (contradictions) | Flag during consolidation; prefer recent |

Every stage emits a `ProcessorOutput` with tokens added/removed and items included/excluded. When behavior regresses, the pipeline log tells you exactly what went in and out.

Details: [`docs/07-context-pipeline.md`](docs/07-context-pipeline.md).

---

## 9. Memory ([`moa-memory`](moa-memory/))

The memory layer is a **file-backed wiki** with a Postgres FTS index. Files are the source of truth — the DB is an index you can rebuild any time.

### Scopes

- **User memory** (`~/.moa/memory/`) — preferences, cross-project learnings, communication style. Travels with the user.
- **Workspace memory** (`~/.moa/workspaces/{id}/memory/`) — project architecture, conventions, decisions, skills. Shared across users in that workspace.

### Structure

```
memory/
├── MEMORY.md           # ≤200-line index, loaded every session
├── _schema.md          # wiki conventions
├── _log.md             # append-only change log
├── topics/             # conceptual pages
├── entities/           # concrete things (services, APIs, people)
├── decisions/          # timestamped ADRs
├── skills/             # Agent Skills (auto-distilled)
└── sources/            # summaries of ingested documents
```

Each page has YAML frontmatter (type, confidence, related, sources, tags, reference_count, last_referenced) and a markdown body.

### Learning loop

- **Correction capture** — user corrects the agent → page updated.
- **Discovery filing** — agent finds something worth remembering → writes page.
- **Skill distillation** — successful run with ≥5 tool calls → auto-generated `SKILL.md`.
- **Consolidation ("Dream")** — cron job that normalizes dates, resolves contradictions, prunes stale entries, decays confidence on unreferenced pages, and keeps `MEMORY.md` under 200 lines.

### Concurrent writes

In cloud mode multiple brains may edit the same workspace memory. Each brain writes to its own branch directory (`memory/.branches/brain-<id>/`), and a reconciler cron merges branches — LLM-resolved when there are real conflicts.

Details: [`docs/04-memory-architecture.md`](docs/04-memory-architecture.md).

---

## 10. Hands and MCP ([`moa-hands`](moa-hands/))

Hands are **cattle, not pets**:

- Provisioned lazily on first tool call.
- Paused when idle (Daytona auto-stop).
- Destroyed at terminal session exit (done, failed, cancelled, panicked).

`ToolRouter` decides *where* a tool runs:

- **Built-in** — memory tools, web search, web fetch — run in-process.
- **Hand** — bash, file_*, file_search — routed to a `HandProvider`.
- **MCP** — anything from a configured MCP server — routed through the credential proxy.

Sandbox tiers:

| Tier | Isolation | When |
|---|---|---|
| 0 | None | Built-in memory / search tools |
| 1 | Container (Docker/Daytona) | Default for cloud code execution |
| 2 | MicroVM (Firecracker / E2B) | Untrusted code, security-critical |

Tier 1 hardening applies `no-new-privileges`, read-only rootfs except `/workspace`, dropped capabilities, seccomp blocks for dangerous syscalls, and `iptables` rules that block cloud metadata endpoints.

Details: [`docs/06-hands-and-mcp.md`](docs/06-hands-and-mcp.md).

---

## 11. Communication layer ([`moa-gateway`](moa-gateway/))

All platforms normalize to a common `InboundMessage` / `OutboundMessage` pair. `PlatformRenderer` converts the common format to platform-native output, respecting each platform's message size, button model, edit window, and rate limit.

Session ↔ thread mapping:

- **Telegram:** reply chain, pinned status message edited in-place.
- **Slack:** thread with live parent; App Home dashboard for multi-session.
- **Discord:** auto-created thread with embed + ActionRow controls.

**Three-tier approvals** on every risky action — `Allow Once`, `Always Allow`, `Deny`. "Always Allow" rules match at the **parsed command level**, not the raw string, so `bash: npm test` does not implicitly approve `npm test && rm -rf /`.

**Three observation verbosities** — `Summary` / `Normal` / `Verbose`. Throttled to each platform's edit interval to stay within rate limits.

Details: [`docs/03-communication-layer.md`](docs/03-communication-layer.md).

---

## 12. Security ([`moa-security`](moa-security/))

**Default posture:**

- **Local:** usable by default — present user can observe and intervene.
- **Cloud:** secure by default — per-workspace tool enablement, approvals for write/exec, containers mandatory.

**Credential isolation — the core rule:** credentials never enter the sandbox where LLM-generated code runs. Two patterns carry this:

1. **Bundled with resource** — e.g. a Git token embedded into a clone URL at provisioning time so the agent can `git push` without ever seeing the token.
2. **MCP credential proxy** — the brain holds opaque session-scoped tokens. The proxy swaps tokens for real credentials just before dispatching to the external service. The brain never sees API keys, OAuth tokens, or passwords.

**Prompt injection defense in depth:**

- Layer 1 — heuristic + canary classification of untrusted content.
- Layer 2 — instruction hierarchy (system > user memory > workspace memory > skills > tool results).
- Layer 3 — tool permission policies (allow / deny / require approval, matched at parsed-command level).
- Layer 4 — canary tokens; appearance in a tool call signals manipulation.

Standards alignment: OWASP Top 10 for Agentic Apps 2026, NIST AI Agent Standards, Least-Agency principle.

Details: [`docs/08-security.md`](docs/08-security.md).

---

## 13. Skills and learning ([`moa-skills`](moa-skills/))

MOA adopts the **Agent Skills** format (agentskills.io) with MOA-specific metadata. Each skill is a directory:

```
skills/deploy-to-fly/
├── SKILL.md          # YAML frontmatter + instructions
├── scripts/          # executable — run, don't read
├── references/
└── assets/
```

**Progressive disclosure:**

| Tier | Content | Loaded when |
|---|---|---|
| Metadata | name, description, tags | Every turn (Stage 4) |
| Instructions | Full `SKILL.md` body | When activated for a task |
| Resources | scripts / references / assets | When executing |

**Lifecycle:**

1. **Distill** — ≥5-tool-call successful run → LLM generates `SKILL.md`.
2. **Activate** — Stage 4 surfaces metadata; brain reads full body via `memory_read`.
3. **Improve** — if a better approach is used, the skill is versioned up and `improved_from` is recorded.
4. **Decay** — consolidation lowers confidence on long-unused skills, flags low success rates, prunes references to deleted tools.

Details: [`docs/09-skills-and-learning.md`](docs/09-skills-and-learning.md).

---

## 14. Observability

- **Tracing** via `tracing` + `tracing-subscriber`. Every pipeline stage, tool call, and LLM request is a span.
- **OTel export** via `tracing-opentelemetry` + `opentelemetry-otlp`. Config lives under `[observability]` in `config.toml`.
- **Custom OTLP headers** supported for direct Langfuse OTLP/HTTP export (see `docs/sample-config.toml`).
- **Cache-ratio logging** — `CacheOptimizer` reports prefix-tokens / total-tokens every turn; regressions in cache hit rate surface immediately.

---

## 15. Technology stack

| Layer | Crate(s) |
|---|---|
| Async runtime | `tokio`, `tokio-util` |
| Serialization | `serde`, `serde_json`, `toml` |
| IDs / time | `uuid` (v7), `chrono` |
| DB | `sqlx` (Postgres) |
| LLM | `async-openai`, `reqwest`, `eventsource-stream`, `tiktoken-rs` |
| Messaging | `teloxide`, `serenity`, `slack-morphism-rust` |
| Desktop | `gpui`, `gpui-component`, `tray-icon`, `pulldown-cmark`, `syntect`, `similar` |
| Orchestration | `restate-sdk`, `tokio-cron-scheduler` |
| Security | `age`, `secrecy`, `vaultrs`, `shell-words` |
| Hands / MCP | `bollard`, `reqwest`, MCP (SDK or custom) |
| Errors / CLI / config | `thiserror`, `anyhow` (bins only), `clap`, `config` |
| Observability | `tracing`, `tracing-subscriber`, `tracing-appender`, `opentelemetry`, `tracing-opentelemetry` |

Full list with versions: [`docs/10-technology-stack.md`](docs/10-technology-stack.md).

---

## 16. Where to look when things break

| Symptom | First stop |
|---|---|
| Brain won't resume after crash | [`docs/11-event-replay-runbook.md`](docs/11-event-replay-runbook.md) |
| Context got huge / cost spiked | [`docs/07-context-pipeline.md`](docs/07-context-pipeline.md) — check stage logs & cache ratio |
| Tool call rejected unexpectedly | [`docs/08-security.md`](docs/08-security.md) — policy evaluation order, canary checks |
| Memory writes conflicting in cloud | [`docs/04-memory-architecture.md`](docs/04-memory-architecture.md) — branch reconciler |
| Approval not reaching the user | [`docs/03-communication-layer.md`](docs/03-communication-layer.md) — platform rate limits, edit window |
| Hand won't provision / destroy | [`docs/06-hands-and-mcp.md`](docs/06-hands-and-mcp.md) — orchestrator cleanup contract |
| Session orchestration stuck | [`docs/02-brain-orchestration.md`](docs/02-brain-orchestration.md) — state transitions, awakeables, runtime wiring |

---

## 17. Design values

Kept here because they explain a lot of surprising code decisions:

- **Inspectability over magic.** If you can't see what the agent did, the fix for the next regression is guesswork.
- **Reversible collaboration.** The human stays in control: inspect → approve → checkpoint → revert.
- **Complexity must justify itself.** If a feature doesn't improve daily use, it doesn't ship in the default path.
- **Daily-driver UX beats impressive demos.** Predictable, low-friction, no cognitive fatigue.
- **Model/provider flexibility.** No vendor lock-in; the `LLMProvider` trait is small on purpose.

For product identity and competitive positioning: [`docs/00-direction.md`](docs/00-direction.md).

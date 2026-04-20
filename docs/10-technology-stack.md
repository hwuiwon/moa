# 10 — Technology Stack

_Crates, external services, implementation phases, build and deployment._

---

## Rust crate dependencies

### Core runtime

| Crate | Purpose | Version policy |
|---|---|---|
| `tokio` | Async runtime | Latest stable |
| `serde` + `serde_json` | Serialization | Latest stable |
| `uuid` | ID generation | Latest stable, with `v7` feature |
| `chrono` | DateTime handling | Latest stable |
| `tracing` + `tracing-subscriber` | Structured logging | Latest stable |
| `opentelemetry` + `tracing-opentelemetry` | OTel integration | Latest stable |
| `thiserror` | Error types | Latest stable |
| `anyhow` | Error context (binary crates only) | Latest stable |
| `config` | Configuration loading (TOML) | Latest stable |
| `clap` | CLI argument parsing (derive) | v4 |

### Database & storage

| Crate | Purpose |
|---|---|
| `sqlx` | SQL toolkit for Postgres access, migrations, and typed queries |

### LLM providers

| Crate | Purpose |
|---|---|
| `async-openai` | OpenAI Responses API client |
| `reqwest` | HTTP client for Anthropic + Google Gemini APIs |
| `eventsource-stream` | SSE parsing for streaming responses |
| `tiktoken-rs` | Token counting (OpenAI tokenizer) |

### Messaging

| Crate | Purpose |
|---|---|
| `teloxide` | Telegram Bot API (async, dptree-based) |
| `serenity` | Discord API (with Gateway + HTTP, auto-sharding) |
| `slack-morphism-rust` | Slack API (Web, Events, Socket Mode, Block Kit) |

### Desktop app

| Crate | Purpose |
|---|---|
| `gpui` | Desktop UI framework |
| `gpui-component` | Reusable desktop controls and layout |
| `tray-icon` | System tray integration |
| `pulldown-cmark` | Markdown parsing |
| `syntect` | Syntax highlighting |
| `similar` | Diff algorithm |

### Orchestration

| Crate | Purpose |
|---|---|
| `restate-sdk` | Restate Rust SDK |
| `tokio-cron-scheduler` | Local cron jobs (consolidation, etc.) |

### Security

| Crate | Purpose |
|---|---|
| `age` | File encryption (credential vault) |
| `secrecy` | Zeroize-on-drop secret types |
| `vaultrs` | HashiCorp Vault client (cloud mode) |
| `shell-words` | Shell command parsing (for approval matching) |

### Hands & MCP

| Crate | Purpose |
|---|---|
| `bollard` | Docker API client (local container hands) |
| `reqwest` | HTTP client for Daytona/E2B APIs |
| `mcp-sdk` | MCP client (if available) or custom implementation |

---

## External services

### Required for cloud mode

| Service | Purpose | Cost |
|---|---|---|
| Kubernetes | Control plane for Restate + MOA workloads | Varies by cluster |
| Restate | Durable orchestration engine | Cluster-hosted |
| Neon / managed Postgres | Session database and checkpoint branches | Varies by plan |
| Daytona | Container hands (default) | ~$0.067/hr per container |
| LLM API | Anthropic, OpenAI, or Google Gemini | Pay-per-token |

### Required for local mode

| Service | Purpose | Cost |
|---|---|---|
| LLM API | Anthropic, OpenAI, or Google Gemini | Pay-per-token |
| Docker + Postgres 18 | Required local storage backend | Free |

### Optional

| Service | Purpose |
|---|---|
| E2B | MicroVM hands (Tier 2 security) |
| HashiCorp Vault | Cloud credential management |
| Grafana Cloud | Observability dashboards (OTel export) |

---

## Implementation phases

### Phase 1: Core loop (4-6 weeks)

**Goal**: A working local agent you can chat with in a terminal.

Deliverables:
- [ ] Rust workspace scaffold (all crate directories)
- [ ] `moa-core`: types, traits, config, error handling
- [ ] `moa-providers`: Anthropic provider (streaming completion)
- [ ] `moa-session`: PostgresSessionStore
- [ ] `moa-brain`: Brain harness loop (single turn: compile → call LLM → emit events)
- [ ] `moa-brain/pipeline`: All 7 context compilation stages (basic implementations)
- [ ] `moa-memory`: FileMemoryStore (MEMORY.md read/write, FTS5 search)
- [ ] `moa-orchestrator`: LocalOrchestrator (tokio tasks + mpsc channels)
- [ ] `moa-desktop`: Basic chat view (prompt input, streaming output, no sidebar)
- [ ] `moa-cli`: one-shot and admin entry points

Test: `moa "What's 2+2?"` works. Agent can read/write memory. Sessions persist across restarts.

### Phase 2: Tools & hands (3-4 weeks)

**Goal**: Agent can execute tools and the user can approve/deny.

Deliverables:
- [ ] `moa-hands`: LocalHandProvider (direct exec + Docker)
- [ ] `moa-hands`: ToolRouter with tool registry
- [ ] Built-in tools: bash, file_read, file_write, file_search, web_search, web_fetch
- [ ] Memory tools: memory_search, memory_write
- [ ] Approval flow: inline approval cards in the desktop app
- [ ] Permission policies: per-workspace rules storage
- [ ] `moa-security`: Basic tool policy checking
- [ ] Desktop app: Tool call cards, approval widgets, diff preview

Test: `moa "Create a hello world Express app"` → agent writes files, user approves, files exist on disk.

### Phase 3: Restate + Kubernetes (3-4 weeks)

**Goal**: Agent runs in the cloud with durable execution.

Deliverables:
- [ ] `moa-orchestrator`: Restate-backed orchestrator binary
- [ ] Restate services, virtual objects, and workflows for session orchestration
- [ ] Kubernetes deployment config (`Dockerfile`, `k8s/`)
- [ ] `moa-session`: Managed Postgres / Neon configuration
- [ ] `moa-hands`: DaytonaHandProvider
- [ ] Multi-session support in orchestrator
- [ ] Session observation (event streaming)
- [ ] `moa --cloud` startup mode

Test: Start a cloud session through the deployed gateway. Kill one orchestrator pod. Restate replays the invocation and the session resumes from the last durable event.

### Phase 4: Messaging gateway (3-4 weeks)

**Goal**: Users can talk to MOA through Telegram, Slack, Discord.

Deliverables:
- [ ] `moa-gateway`: PlatformAdapter trait
- [ ] `moa-gateway`: Telegram adapter (teloxide)
- [ ] `moa-gateway`: Slack adapter (slack-morphism, Block Kit)
- [ ] `moa-gateway`: Discord adapter (serenity)
- [ ] Platform-adaptive message rendering
- [ ] Approval buttons per platform (three-tier)
- [ ] Thread observation UX per platform (status messages, throttled updates)
- [ ] Queue message handling
- [ ] Stop/cancel via platform buttons
- [ ] Session ↔ platform thread mapping

Test: Send "deploy to staging" in Telegram. Agent provisions a hand, asks for approval via inline buttons, user taps Allow, agent executes, reports back.

### Phase 5: Learning loop (3-4 weeks)

**Goal**: MOA gets smarter with use.

Deliverables:
- [ ] `moa-skills`: Skill registry, Agent Skills format parser
- [ ] Skill distillation from successful runs
- [ ] Skill self-improvement during use
- [ ] Wiki compilation (ingest sources → update entity/topic/decision pages)
- [ ] Memory consolidation workflow (Restate workflow + delayed send)
- [ ] Git-branch concurrent writes + LLM reconciler
- [ ] Memory per-user scoping
- [ ] Desktop app: Memory browser (two-pane wiki view)
- [ ] Pipeline Stage 4 (SkillInjector) with progressive loading

Test: Complete 3 complex tasks. Check that skills were auto-generated. Start a new session. Verify the agent uses the skills instead of solving from scratch.

### Phase 6: Polish & hardening (2-3 weeks)

**Goal**: Production-ready.

Deliverables:
- [ ] `moa-providers`: OpenAI + Google Gemini providers
- [ ] `moa-hands`: E2B provider (Tier 2 microVM)
- [ ] `moa-hands`: MCP client + credential proxy
- [ ] `moa-security`: Full credential vault (local + HashiCorp)
- [ ] `moa-security`: Prompt injection detection
- [ ] `moa-security`: Canary tokens
- [ ] Desktop app: Full sidebar, tab bar, and keyboard shortcuts
- [ ] Desktop app: Settings panel, workspace switcher, session picker
- [ ] CLI: All subcommands (status, sessions, memory, doctor, daemon, eval)
- [ ] Observability: OTel traces for pipeline stages, tool calls, LLM requests
- [ ] `moa daemon` for persistent local background operation
- [ ] Documentation: README, getting started guide, configuration reference
- [ ] Integration tests: end-to-end session lifecycle
- [ ] Security audit

Test: Full end-to-end: install → configure → chat locally → deploy to cloud → interact via Telegram → observe sessions → review memory → verify skills compound.

---

## Build & distribution

### Local binary

```bash
# Development
cargo build

# Release (optimized, stripped)
cargo build --release
strip target/release/moa

# With specific features
cargo build --release --features "telegram,slack,discord"

# Local clients
cargo build --release -p moa-cli
cargo build --release -p moa-desktop
```

### Docker (cloud deployment)

```dockerfile
FROM rust:1.94.1-trixie AS builder
WORKDIR /app
COPY . .
RUN cargo build --release --features "cloud"

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/moa /usr/local/bin/moa
ENTRYPOINT ["moa", "--cloud"]
```

### Kubernetes deployment

```bash
kubectl apply -k k8s/
```

---

## Environment variables

```bash
# LLM providers (at least one required)
ANTHROPIC_API_KEY=sk-ant-...
OPENAI_API_KEY=sk-...
GOOGLE_API_KEY=AIza...

# Cloud mode (optional)
MOA__DATABASE__URL=postgres://moa:moa@localhost:5432/moa
RESTATE_ADMIN_URL=http://moa-restate.moa-system.svc.cluster.local:9070

# Hands (optional, cloud mode)
DAYTONA_API_KEY=...
E2B_API_KEY=...

# Messaging (optional, cloud mode)
TELEGRAM_BOT_TOKEN=...
SLACK_BOT_TOKEN=xoxb-...
SLACK_APP_TOKEN=xapp-...
DISCORD_BOT_TOKEN=...

# Optional
VAULT_ADDR=https://vault.example.com
VAULT_TOKEN=...
```

---

## Minimum system requirements

### Local mode
- OS: macOS, Linux, Windows (WSL2)
- Rust: 1.80+
- RAM: 256MB (MOA process) + LLM API calls
- Disk: ~50MB (binary) + session/memory storage
- Docker: Optional (for container hands)

### Cloud mode
- Kubernetes cluster
- Restate cluster
- Managed Postgres / Neon database
- Daytona account (pay-per-use)
- At least one LLM API key

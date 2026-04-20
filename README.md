# MOA
[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/hwuiwon/moa)

**A cloud-first, learning, general-purpose AI agent platform — written in Rust.**


MOA is a persistent agent that lives in the cloud, reaches you through the messaging apps you already use, and gets better the longer it runs. Think **many brains, many hands**: stateless reasoning loops recover from a durable event log, and pluggable execution environments (Docker, Daytona, E2B microVMs, MCP servers) carry out the work.

A zero-setup **desktop app and CLI** give you the same experience locally — the brain harness is identical in both modes; only the orchestrator and hand providers swap out.

> Status: early / active development. The architecture is stable, the surface area still moves. Pinning a commit is a good idea for now.

---

## Why MOA

- **Durable sessions.** An append-only Postgres event log survives any brain, hand, or machine crash. Sessions resume from the last event, not from scratch.
- **Stateless brains, pluggable hands.** Brains don't hold state; hands are cattle, not pets. Provisioned lazily, destroyed at session end.
- **Inspectable by design.** Every tool call, context compilation stage, memory write, and LLM request is observable. No magic.
- **Reversible collaboration.** Inspect → approve → checkpoint → revert. Three-tier approval buttons (Allow Once / Always Allow / Deny) on every risky action.
- **Learning loop.** A file-backed wiki memory (markdown on disk, Postgres FTS index) compounds with every session. Skills are auto-distilled from successful multi-step runs.
- **Model-agnostic.** Anthropic, OpenAI, and Google Gemini are first-class. No vendor lock-in.
- **Messaging-first.** Telegram, Slack, and Discord adapters are part of the core communication layer, not an afterthought.

---

## Quickstart (local)

**Prerequisites:** Rust 1.80+, Docker, and at least one LLM API key (Anthropic, OpenAI, or Google).

```bash
# 1. Start Postgres (the only required local service)
make dev            # equivalent to: docker compose up -d

# 2. Provide an API key
export ANTHROPIC_API_KEY=sk-ant-...
# or OPENAI_API_KEY=...  / GOOGLE_API_KEY=...

# 3. Build and initialize
cargo build
cargo run --bin moa -- init
cargo run --bin moa -- doctor    # sanity-check your environment

# 4. One-shot prompt
cargo run --bin moa -- exec "What's 2+2?"

# 5. Or launch the desktop app (built separately — GPUI)
cargo run -p moa-desktop
```

Everything lives under `~/.moa/` (memory, sandbox, logs, vault).

---

## CLI surface

```
moa exec "<prompt>"              # one-shot, streams events to stderr, result to stdout
moa status                       # active daemon/session status
moa sessions [--workspace .]     # list persisted sessions
moa memory search <query>        # FTS search across workspace memory
moa memory show <path>           # render one memory page
moa memory ingest <files...>     # ingest documents into workspace memory
moa config [set <key> <value>]   # read / update config
moa init                         # initialize MOA directories in the current workspace
moa doctor                       # local environment diagnostic
moa daemon {start|stop|status|logs}
moa checkpoint {create|list|rollback|cleanup}
moa eval {run|plan|skill|list}
moa version
```

The desktop app is a separate binary:

```bash
cargo run -p moa-desktop
```

See [`docs/03-communication-layer.md`](docs/03-communication-layer.md) for the full interaction model (keybindings, approval UX, slash commands, observation verbosity).

---

## Configuration

Config lives at `~/.moa/config.toml`. A commented reference copy is at [`docs/sample-config.toml`](docs/sample-config.toml). Key sections:

| Section | Controls |
|---|---|
| `[general]` | default provider, model, reasoning effort |
| `[providers.*]` | per-provider API key env vars |
| `[database]` | Postgres connection string (required) |
| `[local]` | sandbox dir, memory dir, Docker preference |
| `[cloud]` | cloud-mode enablement and hand-provider settings |
| `[gateway]` | Telegram / Slack / Discord tokens |
| `[permissions]` | default posture, auto-approve list, deny list |
| `[compaction]` | event threshold, preserve-errors, recent-turns-verbatim |
| `[observability]` | OTLP endpoint, sampling, custom headers |

Environment variables override TOML via the `MOA__` prefix (e.g. `MOA__DATABASE__URL`, `MOA__CLOUD__ENABLED=true`).

---

## Deploying to the cloud

Production deployment uses **Restate** for durable orchestration, Kubernetes manifests under [`k8s/`](k8s/) for runtime deployment, and **Daytona** (or E2B) for container/microVM hands.

```bash
# Build the production image
docker build -t moa:latest .

# Deploy
kubectl apply -k k8s/
```

Required environment in cloud mode:

```bash
MOA__CLOUD__ENABLED=true
MOA__DATABASE__URL=postgres://...            # managed Postgres / Neon
RESTATE_ADMIN_URL=http://moa-restate.moa-system.svc.cluster.local:9070
DAYTONA_API_KEY=...                          # or E2B_API_KEY
ANTHROPIC_API_KEY=...                        # at least one LLM provider
TELEGRAM_BOT_TOKEN=...                       # optional messaging channels
SLACK_BOT_TOKEN=...  SLACK_APP_TOKEN=...
DISCORD_BOT_TOKEN=...
```

Full cloud-deployment details: [`docs/02-brain-orchestration.md`](docs/02-brain-orchestration.md) and [`docs/10-technology-stack.md`](docs/10-technology-stack.md).

---

## Architecture at a glance

```
Messaging  ─┐
Desktop    ─┼─►  Gateway  ─►  Brain Orchestrator  ─►  Brain (stateless harness)
CLI        ─┘                  (Restate | Local)            │
                                                            ├─► LLM Provider
                                                            │   (Anthropic / OpenAI / Gemini)
                                                            │
                                                            ├─► Hands (pluggable)
                                                            │   Docker │ Daytona │ E2B │ MCP
                                                            │
                                                            └─► Session Log (Postgres, append-only)
                                                                └─► Memory (file-wiki + FTS)
```

The same trait hierarchy — `BrainOrchestrator`, `SessionStore`, `HandProvider`, `LLMProvider`, `PlatformAdapter`, `MemoryStore`, `ContextProcessor` — powers both modes. Only the concrete implementations swap.

For the full picture, read **[`architecture.md`](architecture.md)**. For runtime flow diagrams (mermaid), see **[`sequence-diagrams.md`](sequence-diagrams.md)**.

---

## Workspace layout

| Crate | Role |
|---|---|
| [`moa-core`](moa-core/) | Core types, traits, config, errors — the interface seam |
| [`moa-brain`](moa-brain/) | Brain harness loop and 7-stage context compilation pipeline |
| [`moa-session`](moa-session/) | `PostgresSessionStore` — append-only event log, replay, compaction |
| [`moa-memory`](moa-memory/) | File-wiki memory, FTS index, ingestion, consolidation, git-branch concurrent writes |
| [`moa-hands`](moa-hands/) | `LocalHandProvider`, Daytona, E2B, MCP client, tool router |
| [`moa-providers`](moa-providers/) | Anthropic, OpenAI, Gemini LLM providers with streaming + caching |
| [`moa-orchestrator`](moa-orchestrator/) | `LocalOrchestrator` (tokio) and the Restate-backed orchestrator binary |
| [`moa-gateway`](moa-gateway/) | Telegram / Slack / Discord adapters, platform-adaptive rendering, approvals |
| [`moa-security`](moa-security/) | Credential vault, MCP proxy, sandbox policies, injection detection |
| [`moa-skills`](moa-skills/) | Agent Skills registry, distillation, self-improvement |
| [`moa-eval`](moa-eval/) | Evaluation harness for agents, skills, and suites |
| [`moa-runtime`](moa-runtime/) | Shared runtime wiring (local + cloud bootstrap) |
| [`moa-cli`](moa-cli/) | `moa` binary — CLI + daemon entrypoints |
| [`moa-desktop`](moa-desktop/) | GPUI desktop application (not a default workspace member) |

---

## Development

```bash
make dev            # start Postgres
cargo build         # default workspace build (excludes moa-desktop)
cargo test          # run tests
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all

cargo build -p moa-desktop   # GPUI app — built explicitly
```

House rules (see [`AGENTS.md`](AGENTS.md) for the full set):

- `thiserror` for library errors; `anyhow` only in binary entrypoints
- `tracing` for all logging — never `println!`/`eprintln!` in library code
- All I/O is async; `tokio` is the runtime
- No `unwrap()` in library code
- Every public function and module carries a doc comment
- `cargo clippy` and `cargo fmt` must pass before any step is "done"

---

## Documentation

Short description of each file in [`docs/`](docs/):

| File | Covers |
|---|---|
| [`00-direction.md`](docs/00-direction.md) | Product identity, philosophy, target users |
| [`01-architecture-overview.md`](docs/01-architecture-overview.md) | System diagram, trait definitions, workspace layout |
| [`02-brain-orchestration.md`](docs/02-brain-orchestration.md) | Restate orchestration, local runtime mode, brain loop |
| [`03-communication-layer.md`](docs/03-communication-layer.md) | Gateway, desktop/CLI, approvals, thread observation |
| [`04-memory-architecture.md`](docs/04-memory-architecture.md) | File-wiki, FTS, consolidation, concurrent writes |
| [`05-session-event-log.md`](docs/05-session-event-log.md) | Postgres schema, event types, compaction, replay |
| [`06-hands-and-mcp.md`](docs/06-hands-and-mcp.md) | `HandProvider`, Daytona, E2B, MCP, tool routing |
| [`07-context-pipeline.md`](docs/07-context-pipeline.md) | 7-stage context compilation and cache optimization |
| [`08-security.md`](docs/08-security.md) | Credential vault, sandbox tiers, prompt-injection mitigation |
| [`09-skills-and-learning.md`](docs/09-skills-and-learning.md) | Agent Skills format, distillation, self-improvement |
| [`10-technology-stack.md`](docs/10-technology-stack.md) | Crates, external services, phases, deployment |
| [`11-event-replay-runbook.md`](docs/11-event-replay-runbook.md) | Operational runbook for event replay |
| [`12-restate-architecture.md`](docs/12-restate-architecture.md) | Restate services, virtual objects, workflows, and Kubernetes deployment |
| [`14-post-migration-notes.md`](docs/14-post-migration-notes.md) | Final migration notes, cost comparison, and follow-up debt |

---

## License

Apache-2.0. See [`LICENSE`](LICENSE).

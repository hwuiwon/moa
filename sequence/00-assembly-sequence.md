# MOA Assembly Sequence

_Build order from empty repo to production. 23 steps, each with a self-contained prompt._

---

## Dependency graph

```
Step 01: Scaffold ──────────────────────────────────────────────────────┐
    │                                                                    │
Step 02: Session Store ─────────────────────────────────┐               │
    │                                                    │               │
Step 03: Anthropic Provider ────────────┐               │               │
    │                                    │               │               │
Step 04: Brain Harness + Pipeline ──────┤               │               │
    │                                    │               │               │
Step 05: Memory Store + FTS5 ───────────┤               │               │
    │                                    │               │               │
Step 06: Tool Registry + Local Hand ────┤               │               │
    │                                    │               │               │
Step 07: Tool Router + Approvals ───────┤               │               │
    │                                    │               │               │
Step 08: CLI + TUI Chat ───────────────┐│               │               │
    │                                   ││               │               │
Step 09: TUI Approval + Diff ──────────┤│               │               │
    │                                   ││               │               │
Step 10: LocalOrchestrator ────────────┤│               │               │
    │                                   ││               │               │
Step 11: TUI Sessions + Observe ───────┘│               │               │
    │                                    │               │               │
Step 12: Skills System ─────────────────┘               │               │
    │                                                    │               │
Step 13: Consolidation + Wiki ──────────────────────────┘               │
    │                                                                    │
Step 14: Telegram ──────────────────────┐                               │
Step 15: Slack ─────────────────────────┤                               │
Step 16: Discord + Platform UX ─────────┘                               │
    │                                                                    │
Step 17: Temporal Orchestrator ─────────────────────────────────────────┤
Step 18: Daytona + E2B + MCP ───────────────────────────────────────────┤
Step 19: Security Hardening ────────────────────────────────────────────┤
Step 20: OpenAI + OpenRouter ───────────────────────────────────────────┤
Step 21: TUI Polish ────────────────────────────────────────────────────┤
Step 22: CLI + Daemon + Observability ──────────────────────────────────┤
Step 23: Cloud Deployment ──────────────────────────────────────────────┘
```

## Milestones

| After step | You can... |
|---|---|
| 04 | Chat with an LLM in a test harness (no UI) |
| 08 | Chat in a terminal TUI with tool execution |
| 11 | Manage multiple sessions, observe running agents, queue/stop |
| 13 | Agent learns from usage, memory compounds across sessions |
| 16 | Users interact via Telegram, Slack, Discord |
| 23 | Full production deployment with cloud infrastructure |

## How to use the prompts

1. Place the spec files (`00-direction.md` through `10-technology-stack.md`) in `docs/` at the repo root
2. Place `AGENTS.md` at the repo root (created alongside the prompts)
3. Feed each prompt to your LLM coding agent in order
4. After each step, run the acceptance tests before proceeding
5. Each prompt assumes all previous steps are complete and passing

## Repo structure after Step 01

```
moa/
├── AGENTS.md                     # Instructions for the implementing LLM
├── docs/                         # Architecture spec (00-10)
├── Cargo.toml                    # Workspace root
├── moa-core/
├── moa-brain/
├── moa-session/
├── moa-memory/
├── moa-hands/
├── moa-providers/
├── moa-orchestrator/
├── moa-gateway/
├── moa-tui/
├── moa-cli/
├── moa-security/
└── moa-skills/
```

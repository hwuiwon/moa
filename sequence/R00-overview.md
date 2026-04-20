# R00 — Restate Buildout Overview

## Purpose

This prompt pack describes the Restate-based orchestrator buildout for MOA.
Execute prompts R01–R11 in sequence. Each prompt is self-contained and can be
worked independently once its prerequisites are satisfied.

Do not execute any code in this prompt. Read, understand, commit.

## What you are about to do

Build `moa-orchestrator` as the single durable execution layer for sessions,
sub-agents, tools, approvals, memory consolidation, Kubernetes deployment, and
observability. The architectural rationale lives in
`docs/12-restate-architecture.md` and `docs/11-v2-architecture.md`; read both
before starting R01.

## Prompt sequence

| # | Title | What it ships |
|---|---|---|
| R01 | Scaffold Restate | `moa-orchestrator` binary compiles with a trivial `Health` service |
| R02 | `SessionStore` Service | Postgres-backed event log exposed as a Restate Service |
| R03 | `LLMGateway` Service | Journaled LLM calls via `ctx.run()` |
| R04 | `ToolExecutor` Service | Idempotency-aware tool execution |
| R05 | `Session` VO: state + lifecycle | `post_message`, `status`, `cancel`, `destroy` handlers |
| R06 | `Session::run_turn` — brain loop | Full turn execution |
| R07 | Approvals via awakeables | `approve` handler, awakeable flow, gateway integration, timeout |
| R08 | `SubAgent` VO | Conversational sub-agents, dispatch limits, result delivery |
| R09 | `Workspace` VO + `Consolidate` Workflow | Scheduled dream cycles via delayed self-send |
| R10 | Kubernetes manifests | `RestateCluster`, `RestateDeployment`, HPA, PDB, graceful shutdown |
| R11 | OTel + Grafana | Tracing wiring, Alloy config, four core dashboards |

## Key decisions locked in for all prompts

1. **Sessions and sub-agents are Virtual Objects**. Conversational by default.
2. **Only Consolidate and IngestSource are Workflows.** Genuine one-shot operations where re-invocation must be impossible.
3. **Tool calls and LLM calls are Services** called from VO handlers.
4. **`Session::run_turn` uses the self-call pattern** so each turn is its own invocation.
5. **Journal retention stays bounded**: short for service calls, longer for approvals and workflows.
6. **Autoscaling in Phases 1–2 is plain HPA on CPU**, no KEDA.
7. **Observability goes to Grafana LGTM via Alloy**.

## Before R01

1. Read `docs/12-restate-architecture.md` end to end.
2. Read `docs/11-v2-architecture.md` for v2 context.
3. Read `docs/02-brain-orchestration.md` for current lifecycle and local runtime behavior.
4. Read `docs/05-session-event-log.md` for the Postgres event schema.
5. Skim `docs/07-context-pipeline.md` for the context assembly flow.

Then proceed to R01.

## Milestone gates

Do not start a later R-prompt until the earlier one's acceptance criteria are met.
The sequence is designed so each prompt's integration tests pass before the next begins.

Gate after R09: the full orchestrator compiles, all handlers have unit tests, and a synthetic session from `post_message` through `run_turn` through sub-agent dispatch works against a local `restate-server`.

Gate after R11: observability reaches Grafana. Every span in the synthetic session appears in Tempo, every metric in Mimir, and every log in Loki.

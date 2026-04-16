# MOA Architecture Specification

_Date: 2026-04-09 | Status: Final draft, ready for implementation_

## Reading guide

This specification is split into standalone documents. Each is self-contained but cross-references others where needed. Read `00-direction.md` first for context, then any section relevant to your work.

## Documents

| # | Document | Scope |
|---|----------|-------|
| 00 | [Direction](00-direction.md) | Product identity, philosophy, target users |
| 01 | [Architecture Overview](01-architecture-overview.md) | System diagram, component interactions, trait hierarchy, Rust workspace |
| 02 | [Brain Orchestration](02-brain-orchestration.md) | Temporal workflows, Fly.io hosting, local runtime mode, brain lifecycle |
| 03 | [Communication Layer](03-communication-layer.md) | Messaging gateway, approval UX, thread observation, desktop app, CLI |
| 04 | [Memory Architecture](04-memory-architecture.md) | File-wiki, FTS5, scoping, consolidation, concurrent writes |
| 05 | [Session & Event Log](05-session-event-log.md) | Turso/libSQL, event schema, compaction, replay |
| 06 | [Hands & MCP](06-hands-and-mcp.md) | HandProvider trait, Daytona/E2B/Local, MCP proxy, tool routing |
| 07 | [Context Pipeline](07-context-pipeline.md) | 7-stage compilation, cache optimization, failure modes |
| 08 | [Security](08-security.md) | Credential vault, sandbox tiers, prompt injection, approval policies |
| 09 | [Skills & Learning](09-skills-and-learning.md) | Agent Skills format, distillation, self-improvement |
| 10 | [Technology Stack](10-technology-stack.md) | Crates, dependencies, implementation phases, deployment |

## Decisions register

| # | Topic | Decision |
|---|---|---|
| 1 | Brain orchestration | Temporal.io + Fly.io Machines; `LocalOrchestrator` for zero-cloud local |
| 2 | Skill format | Agent Skills standard (agentskills.io) with MOA extensions |
| 3 | Memory | Hybrid file-wiki + FTS5, per-user + per-workspace scoping |
| 4 | Messaging gateway | Single Rust binary, adapter pattern (teloxide / serenity / slack-morphism) |
| 5 | Session storage | Turso/libSQL everywhere (SQLite locally, Turso Cloud remotely) |
| 6 | Approval UX | Three-tier buttons: Allow Once / Always Allow / Deny |
| 7 | Hand provisioning | Pluggable `HandProvider` trait, Daytona default |
| 8 | Security posture | Secure by default in cloud, usable by default locally |
| 9 | Thread observation | Observe + Stop + Queue at launch; Fork + Inject deferred |
| 10 | Event log | Full payloads for important events, summaries for thinking |
| 11 | Context pipeline | 7-stage compilation with stable prefix caching |
| 12 | Local runtime | Full local mode without any cloud providers |
| 13 | Desktop app | GPUI desktop client with memory browser |
| 14 | LLM providers | OpenAI, Anthropic, Google Gemini at launch |
| 15 | Language | Rust |
| 16 | Concurrent memory writes | Git-branch-per-brain with LLM reconciler cron |

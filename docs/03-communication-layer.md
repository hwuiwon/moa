# 03 — Communication Layer

_Client surfaces, gateway adapters, approvals, and observation._

## Product Surfaces

MOA has several front doors over the same session model:

| Surface | Primary crate | Use |
|---|---|---|
| GPUI desktop | `moa-desktop` | Local interactive application |
| CLI and daemon | `moa-cli`, `moa-runtime`, `moa-orchestrator-local` | Local automation, diagnostics, one-shot prompts |
| REST/gateway | `moa-orchestrator`, `moa-gateway` | Cloud and integration entrypoints |
| Messaging adapters | `moa-gateway` | Telegram, Slack, Discord conversations and approvals |

The interfaces differ in rendering and transport. They all eventually create or address a `SessionId`, append user messages, observe session events, and resolve approvals.

## Message Normalization

Messaging platforms normalize inbound traffic into the shared platform DTOs in `moa-core`:

- platform identity
- user identity and optional MOA user link
- channel or thread reference
- text
- attachments
- reply anchor
- timestamp

Outbound rendering is platform-specific, but the payload model is shared: text, markdown, code blocks, diffs, tool cards, approval requests, and status updates.

## Session Mapping

| Surface | Session mapping |
|---|---|
| Desktop | User opens, creates, resumes, and observes local sessions directly |
| CLI | `moa exec` creates or resumes work through the local runtime; daemon commands keep sessions running in the background |
| REST/gateway | HTTP or gateway request maps to a durable session and calls the cloud orchestrator |
| Telegram | Reply chains or threads map to sessions |
| Slack | Slack threads map to sessions |
| Discord | Direct messages or guild threads map to sessions |

The durable state is not stored in the client. Clients can reconnect by replaying Postgres events and, in cloud mode, querying Restate status.

## Approvals

Approval requests are session events with enough information for any surface to render:

- request ID
- optional Restate awakeable ID
- optional sub-agent ID
- tool name
- risk level
- input summary
- structured prompt data, including diffs and suggested allow patterns

The default actions are:

- Allow once
- Always allow with a scoped rule
- Deny with an optional reason

Approval rules are stored in Postgres through the shared approval rule store. Shell approvals are matched at parsed command boundaries so one approval does not accidentally cover chained commands.

## Observation

Observation is history-first:

1. Load durable events from `PostgresSessionStore`.
2. Render them for the client.
3. Attach to the live stream if the orchestrator has one.

This avoids losing information when a client disconnects or a gateway process restarts. Live observation can include:

- session status changes
- user and assistant messages
- tool calls, results, and errors
- approval requests and decisions
- segment start/completion events
- memory and checkpoint events
- runtime events from the local orchestrator

Clients choose their own verbosity, but durable events are the source of truth.

## Desktop App

`moa-desktop` is a native GPUI app. It is a workspace member but not a default member, so build it explicitly:

```bash
cargo build -p moa-desktop
```

The desktop app is the local rich UI for sessions, memory, approvals, diffs, and settings. It talks to the same local runtime and Postgres store as the CLI.

## CLI

The CLI binary is `moa` from package `moa-cli`.

```bash
cargo run -p moa-cli -- exec "Summarize this repository"
cargo run -p moa-cli -- sessions
cargo run -p moa-cli -- memory search "deployment"
cargo run -p moa-cli -- doctor
```

The daemon mode keeps the local runtime alive for background work and reconnecting clients.

## Messaging Gateway

`moa-gateway` owns platform adapters and renderers for Telegram, Slack, and Discord. Adapters convert platform callbacks into the shared command/event model and render approvals with platform-native controls when available.

Current implementation caveats are documented in `implementation-caveats.md`, especially around callback normalization and outbound routing anchors.

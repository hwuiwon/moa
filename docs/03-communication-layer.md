# 03 — Communication Layer

_Client surfaces, gateway adapters, approvals, and observation._

## Product Surfaces

MOA has several front doors over the same session model:

| Surface | Primary crate | Use |
|---|---|---|
| REST/API | `moa-edge`, `moa-orchestrator` | Cloud, automation, diagnostics, and integration entrypoints |
| Messaging adapters | `moa-gateway` | Slack conversations and approvals |

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
| REST/API | HTTP request maps to a durable session and calls the cloud orchestrator |
| Slack | Slack threads map to sessions |

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
- status snapshots from the Restate-backed orchestrator

Clients choose their own verbosity, but durable events are the source of truth.

## API Automation

Operator and test automation call `moa-edge` public
HTTP routes or direct Restate ingress endpoints. `make dev` starts the
long-running `moa-orchestrator` service with Restate and Postgres, and tests use
`moa-test-support` fixtures or raw `reqwest` calls to exercise the same API
surface.

## Messaging Gateway

`moa-gateway` owns the Slack platform adapter and renderer. The adapter converts Slack callbacks into the shared command/event model and renders approvals with platform-native controls when available.

Current implementation caveats are documented in `implementation-caveats.md`, especially around callback normalization and outbound routing anchors.

# 03 — Communication Layer

_Client surfaces, messaging adapters, approvals, and observation._

## Product Surfaces

MOA has several front doors over the same session model:

| Surface | Primary crate | Use |
|---|---|---|
| REST/API | `moa-edge`, `moa-orchestrator` | Cloud, automation, diagnostics, and integration entrypoints |
| Messaging adapters | `moa-messaging` | Slack conversations, approvals, email notifications, and SMS notifications |

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

This avoids losing information when a client disconnects or a messaging process restarts. Live observation can include:

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

## Messaging Adapters

`moa-messaging` owns messaging adapters and renderers. The current conversation adapter is Slack; it converts Slack callbacks into the shared command/event model and renders approvals with platform-native controls when available. The crate also owns outbound notification connectors such as Postmark email and Twilio SMS. Slack, Postmark, and Twilio sends record provider, HTTP status, provider identifiers, error codes, retry class, and retry hint fields on tracing spans without recording message body content or phone numbers.

Notification connectors are transport clients, not durable schedulers. Caller-owned alert or notification workflows that must survive process restarts should invoke them from Restate handlers or workflows. Twilio and Postmark handle safe API-level rate limits locally by retrying HTTP 429 responses with `Retry-After`; Slack uses the Slack SDK rate-control path and maps exhausted rate limits to MOA's typed `RateLimited` error. Terminal or provider-level failures such as Twilio A2P 10DLC `30034`, Postmark inactive-recipient `ErrorCode` values, and Slack `ok:false` API errors are classified and observed so the durable caller can decide whether a new send is allowed.

Current implementation caveats are documented in `implementation-caveats.md`, especially around callback normalization and outbound routing anchors.

# 03 — Communication Layer

_Client surfaces, messaging adapters, action review, and observation._

## Product Surfaces

MOA has several front doors over the same session model:

| Surface | Primary crate | Use |
|---|---|---|
| REST/API | `moa-edge`, `moa-orchestrator` | Cloud, automation, diagnostics, and integration entrypoints |
| Messaging adapters | `moa-messaging` | Slack conversations, action-review notifications, email notifications, and SMS notifications |

The interfaces differ in rendering and transport. They all eventually create or address a `SessionId`, append user messages, observe session events, and show action-review state.

## Message Normalization

Messaging platforms normalize inbound traffic into the shared platform DTOs in `moa-core`:

- platform identity
- user identity and optional MOA user link
- channel or thread reference
- text
- attachments
- reply anchor
- timestamp

Outbound rendering is platform-specific, but the payload model is shared: text, markdown, code blocks, diffs, tool cards, action-review requests, and status updates.

## Session Mapping

| Surface | Session mapping |
|---|---|
| REST/API | HTTP request maps to a durable session and calls the cloud orchestrator |
| Slack | Slack threads map to sessions |

The durable state is not stored in the client. Clients can reconnect by replaying Postgres events and, in cloud mode, querying Restate status.

## Contact Session Flow

Enterprise integrations authenticate as workspace-admin-or-higher callers to
issue MOA contact JWTs for agent-facing contacts. Initial contact tokens are
low assurance: they can create a contact-bound session, but their scopes and
structured permissions bound them to the configured workspace, agent/session
allowlists, and low-assurance memory operations. A dedicated contact
message route must exist before contact tokens receive a message-send scope.

`Contacts/init_session` creates the durable session with a `contact_id` in
session metadata. The client cannot set trusted caller identity or override the
session contact per message. If a workflow or skill needs higher assurance, it
starts contact-point verification, completes the OTP-style challenge through
the contact service, receives a verified contact token, and calls
`Contacts/promote_session`. Verification can deliver OTP codes only to email
and phone contact points today. Email points use the Postmark-backed email
channel; phone points use the Twilio-backed SMS channel. If provider delivery
fails, the service consumes the challenge before returning the error so an
unsent code cannot later promote the contact. Promotion updates the session
contact and user memory subject while preserving the prior anonymous or
unverified contact link for default verified memory continuity.

## Action Reviews

Workspace-admin action reviews are persisted records with enough information for an admin surface to render:

- review ID and workspace
- durable `ActionEnvelope`
- `ActionReviewPreview` with summary fields and diffs
- status and decision metadata

Admin actions are:

- Clear, which executes the stored request with a fresh tool-call ID.
- Deny, which records the decision and does not execute the action.

Conversation clients do not resolve blocking tool gates. Admin review returns a pending-review tool result to the model and the root or sub-agent workflow continues.

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

`moa-messaging` owns messaging adapters, renderers, and channel-neutral
delivery helpers. The current conversation adapter is Slack; it converts Slack
callbacks into the shared command/event model and renders approvals with
platform-native controls when available. The crate also owns outbound
notification connectors such as Postmark email and Twilio SMS, plus a delivery
sink that routes email and SMS use cases such as contact verification through
those existing connectors. Slack, Postmark, Twilio, and the delivery sink record
provider, HTTP status, provider identifiers, error codes, retry class, and retry
hint fields on tracing spans without recording message body content, OTP codes,
email addresses, or phone numbers.

Notification connectors are transport clients, not durable schedulers. Caller-owned alert or notification workflows that must survive process restarts should invoke them from Restate handlers or workflows. Twilio and Postmark handle safe API-level rate limits locally by retrying HTTP 429 responses with `Retry-After`; Slack uses the Slack SDK rate-control path and maps exhausted rate limits to MOA's typed `RateLimited` error. Terminal or provider-level failures such as Twilio A2P 10DLC `30034`, Postmark inactive-recipient `ErrorCode` values, and Slack `ok:false` API errors are classified and observed so the durable caller can decide whether a new send is allowed.

Current implementation caveats are documented in `implementation-caveats.md`, especially around callback normalization and outbound routing anchors.

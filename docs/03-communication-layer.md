# 03 — Communication Layer

_Client surfaces, messaging adapters, action review, and observation._

## Product Surfaces

MOA has several front doors over the same session model:

| Surface | Primary crate | Use |
|---|---|---|
| REST/API | `moa-edge`, `moa-orchestrator` | Cloud, automation, diagnostics, and integration entrypoints |
| Messaging adapters | `moa-messaging` | Slack conversations, action-review notifications, email notifications, and SMS notifications |

The interfaces differ in rendering and transport. They all eventually create or
address a `SessionId` inside one tenant, append contact or admin/operator
messages, observe session events, and show action-review state.

## Message Normalization

Messaging channels normalize inbound traffic into category-owned shared DTOs,
primarily `moa_core::types::channel` and `moa_core::types::contact`:

- channel
- channel actor and optional channel account or MOA contact link
- concrete route reference such as web chat conversation, Slack thread, email account, or SMS account
- text
- attachments
- reply anchor
- timestamp

Outbound rendering is channel-specific, but the payload model is shared: text, markdown, code blocks, diffs, tool cards, action-review requests, and status updates.

## Session Mapping

| Surface | Session mapping |
|---|---|
| REST/API | HTTP request carries the real `ChannelRef` and maps it to a durable session |
| Slack | Slack conversations or threads map through `session_channel_bindings` |
| Email/SMS | Verified contact-point channel accounts can become the active session delivery route |

The durable state is not stored in the client. Sessions denormalize the current
`channel` and `active_channel_binding_id` for fast listing, while
`session_channel_bindings` stores indexed route lookup keys and historical
delivery routes. Clients can reconnect by replaying Postgres events and, in
cloud mode, querying Restate status.

## Contact Session Flow

Enterprise integrations authenticate as tenant admin/operator callers to issue
MOA contact JWTs for agent-facing contacts. Contacts are end users inside one
tenant; users are admin/operator principals. Initial contact tokens are low
assurance: they can create a contact-bound session, but their scopes and
structured permissions bound them to the configured tenant, agent/session
allowlists, and low-assurance memory operations. Token issuance must request an
explicit non-empty low-assurance scope list and an explicit non-empty agent
allowlist; omitted values fail closed instead of expanding to wildcard access. A
dedicated contact message route must exist before contact tokens receive a
message-send scope.

`Contacts/init_session` creates the durable session with a `contact_id` in
session metadata and a required initial `ChannelRef` route. The
client cannot set trusted caller identity or override the session contact per
message. `Contacts/change_session_channel` can later switch the active route,
closing the previous active binding, inserting the new binding, updating
session metadata, and appending `SessionChannelChanged`. If a skill or execution
run needs higher assurance, it starts contact-point verification, completes the
OTP-style challenge through the contact service, receives a verified contact
token, and calls `Contacts/promote_session`. Verification can deliver OTP codes
only to email and phone contact points today. Email points use the
Postmark-backed email channel; phone points use the Twilio-backed SMS channel.
Verified email and SMS contact points receive channel accounts that can be used
as delivery routes. If provider delivery fails, the service attempts one
compensating challenge consume before returning the error; compensation
failures are observable. Promotion updates the session contact to the canonical
verified contact. Contact memory remains contact-local: the promoted session
does not inherit tenant memory or any other contact's memory by default.

`ContactVerifier` owns the persist/deliver/consume sequence. It
depends on the narrow `ContactOtpDelivery` port for provider delivery, so
contact persistence does not depend on a concrete messaging provider and an
undelivered challenge triggers one observable compensating consume attempt.

## Action Reviews

Tenant action reviews are persisted records with enough information for an
admin surface to render. Tenant-level action policies determine whether actions
are allowed, denied, or queued for review:

- review ID and tenant
- durable `ActionEnvelope`, including its one typed `ActionReviewOwner`
- `ActionReviewPreview` with summary fields and diffs
- status and decision metadata

Admin actions are:

- Clear, which executes the stored request as a new MOA-owned invocation with a
  fresh tool-call ID and no provider tool-use ID.
- Deny, which records the decision and does not execute the action.

Conversation clients do not resolve blocking tool gates. Admin review returns a pending-review tool result to the model and the root or worker workflow continues.

Once the review resolves, its conversational owner receives a typed receipt and
runs one continuation turn, recorded as the deduped
`ActionReviewContinuationRequested` session event and rendered to the model as a
system directive (never a fabricated user message). The Session SSE stream
retargets its terminal turn to that continuation only for a `Coordinator` owner,
because only that continuation produces the visible answer the stream is waiting
for; `Worker` and `ExecutionTask` continuations never retarget a contact stream.

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
- worker progress narration, attention signals, and stale notices
- execution-run start, aggregate progress, exact input requests, and terminal status
- status snapshots from the Restate-backed orchestrator

Clients choose their own verbosity, but durable events are the source of truth.
Browser chat clients send user text and their contact token through
`POST /v1/sessions/{session_id}/messages`. The edge injects the path session id
and forwards admission/progress reads through the `Contacts` service, which
verifies the token, `contact:session:message:send` scope, and session
allowlist. The response is an SSE stream over the same HTTP request: it first
emits an `accepted` frame, then transient `progress` frames from
`Session/progress.active_turn_progress` plus durable `response`, `tool`, or
generic `session_event` frames keyed by event sequence number, and finally a
`done` frame. This lets the browser render the turn without holding a second
live progress connection. `Session/progress` remains the compact
history/recovery projection used by the stream and by reload flows.

A detached `run` uses stable SSE names `execution_started`,
`execution_progress`, `execution_input_request`, `execution_completed`, and
`execution_failed`. Progress is cadence/delta limited and aggregate; the session
event log does not receive one event per task heartbeat or every raw map output.
When an execution task returns user-audience `NeedsInput`, the next matching
reply resolves that exact run/task wait instead of starting an unrelated root
turn. Terminal delivery requests at most one guarded synthesis turn linked to
the originating user sequence and run ID.

When a coordinator turn delegates to workers, the stream stays open across the
**detached window**. `session_message_terminal_done` closes only when the started
turn has completed **and** `Session/progress.child_progress` shows no non-terminal
child; with no children it collapses to the previous turn-completion-only close.
While children run, the stream keeps emitting transient active-turn progress and
durable coordination frames mapped from the new `Event` variants:
`progress_narration` (`ProgressNarrated`, the
primary user-facing liveness, rendered in the assistant's voice), `worker_signal`
(`WorkerSignalReceived`), `worker_resume` (`WorkerParentResumeRequested`),
and `worker_stale` (`WorkerHeartbeatStale`); terminal
`WorkerNotificationDelivered` stays on the generic `session_event` frame. During
event silence with an active descendant, the edge also emits a templated, **non-durable**
`working` frame (active child summary + elapsed seconds) after a fixed 10s
interval, so the user never sees a frozen screen even when narration correctly
skips a no-change period or is disabled. `child_progress` is built by bounded
fan-in (active children only, capped) so the projection stays compact.

This detached-worker window is specific to interactive delegation in `act`.
Conversational `Worker` remains steerable and bounded, but it is not the bulk
DAG primitive. `run` progress and completion come from `ExecutionRun` aggregate
state and never fan in execution tasks through the `Session` virtual object.

Each durable frame's SSE `id` is the event `sequence_num`, which clients use for
their own ordering and dedupe. A reconnect carrying `Last-Event-ID` resumes after
that sequence; a fresh connection seeds its cursor from the current event head.

Every submitted message carries a caller-owned `client_message_id`; MOA never
synthesizes one, and a missing, empty, oversized, or control-character id is a typed
rejection before any session mutation. The Session admission fence is keyed on that id
plus a canonical hash of the request's semantic fields, so a client that retries after a
lost response receives the original response — same turn, same queue position, same
pre-admission cursor — instead of a second paid turn, a second queue entry, a second
reply delivery, or duplicated attachments. Reusing one id for a different request is a
typed conflict. The guarantee is bounded: a terminal admission is retained until the
earlier of 24 hours or 256 newer terminal admissions in that session, after which the id
is admissible again as new work; unresolved and queued admissions are never evicted.

A reconnect is not an exemption from the fence. The edge submits every attempt, including
one carrying `Last-Event-ID`, and then resumes the stream from `Last-Event-ID + 1`; a
first connection stores the cursor it observed before admission and every retry of that
id receives that stored cursor rather than the newer stream head. The transport cursor is
excluded from the semantic request hash.

A message may address one waiting request for user input with an explicit typed
`reply_to`. With no `reply_to`, zero waiting requests is an ordinary turn and exactly one
is a convenience delivery, but several waiting requests is a typed rejection: guessing
which one the user answered could approve the wrong plan or unblock the wrong task. An
explicit target that matches nothing the session is waiting on — including a superseded
execution generation — conflicts without mutation, and a reply cannot carry attachments,
because reply delivery carries text only. An upload is therefore always ordinary work,
never an implicit reply.

The same route accepts multipart contact messages with text, photo uploads, or
both (`client_message_id` is a required part; an explicit `reply_to` is the same JSON
object the JSON body would carry). Upload bytes are validated by the edge and stored
through `object_store` before session admission continues; local development uses RustFS,
while cloud deployments use AWS S3 or GCS. The durable session message carries only
`Attachment` metadata with a `SessionAttachmentId`. Clients reload the session
from events and fetch bytes through the authorized session attachment route.

Each upload occupies a deterministic slot derived from tenant, session, client message
id, and attachment ordinal, so a retried submission addresses the same row and the same
stored object. The metadata row is claimed first and objects are written create-only, so
a retry cannot overwrite stored bytes before Postgres has decided whether it is a replay
or a conflict. A slot holding byte-identical content with identical metadata replays; a
slot whose digest or metadata changed is a typed conflict. Cleanup after a rejected
message deletes only the attachments that request created — never a replayed original
belonging to the message that is still live.

## API Automation

Operator and test automation call `moa-edge` public
HTTP routes or direct Restate ingress endpoints. `make dev` starts the
long-running `moa-orchestrator` service with Restate and Postgres, and tests use
`moa-test-support` fixtures or raw `reqwest` calls to exercise the same API
surface.

Tenant admins and operators may use the stateless Streamable HTTP MCP endpoint
at `/mcp`. Every HTTP message is authenticated by the edge, authorized against
`tenant:<authenticated tenant>#operator`, and then dispatched to an explicit
tool allowlist. Read tools use the same edge read-model functions as the REST
dashboard; command tools use shared wire DTOs and the existing sanitized
edge-to-ingress proxy. MCP clients discover capabilities with `tools/list`; the
server does not mirror them as MCP resources or prompts.

Execution controls use the common typed run APIs: list, start, status, cancel,
review decision, signal delivery, and bounded task-result listing. A start
request identifies a published skill's exact pinned `execution_plan` template
revision plus objective and structured input, then enters the session-originated
planning/admission path. Model-facing clients submit neither a compiled-plan
identifier nor raw graph JSON.

### Edge-to-ingress forwarding

`moa-edge` terminates the caller credential, resolves identity and tenant, and
translates each public `/v1/...` path to a Restate ingress call. It uses the
v1.7 request-response scheme `POST /restate/call/{service}/{handler}` (keyed
form `.../{service}/{key}/{handler}` for the `Session` virtual object), building
the path once in `crate::ingress` so route translation never encodes the wire
contract. The edge issues only request-response calls, never the fire-and-forget
`/restate/send/...` form.

Turn-starting invocations are tagged for per-tenant flow control. Posting a
message (`POST /v1/sessions/{session_id}/messages` →
`Contacts/send_message`) starts a turn, so the edge forwards it on the scoped
form `POST /restate/scope/tenant-{tenant_id}/call/...`. Every cheap read, status
poll (`Session/progress`, `Contacts/progress`), authorization check, and
session-lifecycle call stays unscoped, so a status poll can never wait behind a
tenant's turn concurrency. `docs/12-restate-architecture.md` describes the
cluster rule book and per-scope counters that back this admission control.

## Messaging Adapters

`moa-messaging` owns messaging adapters, renderers, and channel-neutral
delivery helpers. `SlackAdapter` is the public Slack conversation seam; its
adapter, inbound mapping, chunking, reference, and error concerns are split
into Slack-owned modules rather than a new provider-neutral framework. It
converts Slack messages into the shared command/event model and renders
outbound Slack content as plain text or Markdown, including action-review notifications. Interactive
Slack controls are intentionally out of scope; action-review decisions happen
through the durable admin/API review surface. The crate also owns outbound
notification connectors such as Postmark email and Twilio SMS, plus a delivery
sink that routes email and SMS use cases such as contact verification through
those existing connectors. Slack, Postmark, Twilio, and the delivery sink record
provider, HTTP status, provider identifiers, error codes, retry class, and retry
hint fields on tracing spans without recording message body content, OTP codes,
email addresses, or phone numbers.

Notification connectors are transport clients, not durable schedulers. Caller-owned alert or notification workflows that must survive process restarts should invoke them from Restate handlers or workflows. Twilio and Postmark handle safe API-level rate limits locally by retrying HTTP 429 responses with `Retry-After`; Slack uses the Slack SDK rate-control path and maps exhausted rate limits to MOA's typed `RateLimited` error. Terminal or provider-level failures such as Twilio A2P 10DLC `30034`, Postmark inactive-recipient `ErrorCode` values, and Slack `ok:false` API errors are classified and observed so the durable caller can decide whether a new send is allowed.

Slack channel pacing and multi-chunk outbound message references use
`RuntimeCacheStore` when configured. With Redis selected, replicas coordinate
per-channel send slots and edit/delete references. With the memory backend,
those values are per-pod best effort; durable conversation routing still comes
from Postgres session/channel bindings and session events.

Current implementation caveats are documented in `implementation-caveats.md`.

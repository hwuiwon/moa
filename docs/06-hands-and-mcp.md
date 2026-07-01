# 06 - Hands & MCP

_Hand providers, tool routing, MCP, sandbox lifecycle, and recovery._

## Contract

Hands are temporary execution environments. They are provisioned on first use
and destroyed when their owning scope reaches a terminal state. The root
coordinator runs sandbox-free; each worker owns its own hand, so a hand is
released when that worker self-cleans and any remaining hands are released at
session teardown (see Per-Worker Sandbox Model below). The brain never talks
to hands directly; it asks the `ToolRouter` to execute a named tool with
structured input.

Credentials must not be visible to generated code. Git, MCP, and external API
credentials are fetched or injected by trusted host-side code, not placed in
tool-call arguments.

## Provider Map

| Provider | Use | Notes |
|---|---|---|
| Local | Zero-setup tests and development | Uses a workspace directory and optional Docker support. |
| Docker | Local/containerized execution | Hardened by `moa-hands` and `moa-security` policies. |
| Daytona | Default cloud workspace provider | Supports pause/resume/destroy around idle sessions. |
| E2B | MicroVM isolation | Use for untrusted or security-sensitive execution. |
| MCP | External tools and SaaS integrations | Routed through `MCPClient` and the credential proxy. |

All providers implement the `HandProvider` trait from `moa-core`. Tool routing
code should depend on the trait, not on provider-specific clients.

`HandProvider::install_files` is the trusted setup path for files the runtime
must place in a sandbox before model-visible execution. MOA uses it to
materialize selected skill packages under `.moa/skills/<skill>/...`.

## Tool Router

`moa-hands::ToolRouter` owns tool preparation and dispatch:

1. Look up the tool in `ToolRegistry`.
2. Normalize and budget tool input/output.
3. Prepare `ToolPolicyInput`, the suggested action-policy pattern, and
   tenant-admin review preview data.
4. Evaluate action policy.
5. Execute allowed actions through one of:
   - built-in tool handler,
   - cached hand provider,
   - MCP client.
6. Record lineage and route the result back to the turn loop.

The default provider name is `local`. Workspace roots, active hand handles,
MCP clients, action-policy rule stores, session store hooks, and optional
memory executor hooks live behind async locks so the router can be shared
across handlers. These maps are process-local caches or transport internals;
they must not be the source of cross-request correctness in Kubernetes.

`ActionEnvelope` is the durable policy-facing record for one tool invocation.
It includes the review id, tenant, user, session or worker origin, tool
call id, tool name, normalized input, input summary, risk level, action class,
optional workflow/artifact origin metadata, idempotency key, and creation time.
The envelope is persisted only when action policy returns
`ActionPolicyEffect::AdminReview`; normal allowed actions proceed directly.

Action-policy decisions are ordered:

1. Tenant-visible persistent rules match by tool name and normalized input;
   the strictest matching rule wins.
2. Configured `always_deny` and `admin_review` tool-name globs can tighten the
   matched rule result.
3. The stricter of the tool's default effect and the global default effect is
   used when no rule or configured tool policy matches.

`Deny` returns a tool error and the turn continues. `AdminReview` queues a
tenant-admin action review through `ActionReviews/request`, writes an
`ActionReviewRequested` event for session history, returns a pending-review
tool result to preserve LLM protocol continuity, and continues the root or
worker turn without moving the session into a waiting state. Tenant admins
list pending reviews through `ActionReviews/list_pending`. Review
requests are canary-screened before persistence and store no canary token; a
cleared review rewrites the stored tool request with a fresh tool-call id before
invoking `ToolExecutor`, while a denied review records the decision without
executing the tool.

## Registry

Tools come from three sources:

| Source | Examples | Execution |
|---|---|---|
| Built-ins | memory tools, search helpers | In-process Rust handlers |
| Hand tools | `bash`, `file_read`, `file_write`, `file_search` | Local/Docker/Daytona/E2B hand |
| MCP tools | GitHub, browser, database, SaaS tools | MCP transport |

Tool descriptors include name, schema, execution backend, risk level, action
class, default action-policy effect, and output budget. The context pipeline
injects only the currently active subset to protect prompt budget and cache
stability.

## Lifecycle

Active hands are keyed by session, worker, and provider. The authoritative
binding lives in Postgres `moa.hand_leases` with primary key `(session_id,
worker_id, provider)`; `ToolRouter` process maps are reconnect caches only. A
first tool call claims a durable lease before provisioning the hand. Later tool
calls on any Kubernetes replica load the lease, reconnect or resume the provider
handle when healthy, or mark it stale and reprovision with a new generation. On
terminal session status, cancellation, failure, or panic cleanup, the
orchestrator calls `reclaim_hands(session_id, None)`, which lists durable
leases for every worker scope rather than only handles cached in the current
process.

### Per-Worker Sandbox Model

Worker compute is keyed by `worker_id`, not by the parent session, so each
worker owns exactly one sandbox and siblings never share one:

- **The coordinator is sandbox-free.** Root-turn preparation in `brain_bridge.rs`
  filters the coordinator's tool schemas through `ToolRouter::tool_requires_sandbox`
  (true only for `ToolExecution::Hand` tools) and hard-excludes those tools except
  manifest-backed selected-skill `file_read`. The root `file_read` path is served
  directly from the trusted manifest by `ToolExecutor`; it does not provision a
  hand. The worker tool subsets keep the hand tools, so all real computation is
  delegated. Zero workers means zero sandboxes.
- **Each worker owns one hand.** `ToolCallRequest.worker_id` is populated
  from `GovernedInvocationOrigin::Worker` and threaded through the tool executor
  into the lease/cache key `(session_id, worker_id, provider)`. The
  pre-existing coordinator scope is the empty `worker_id`. N parallel
  workers hold N independent sandboxes.
- **Per-worker release.** Because each sandbox has exactly one owner, a child
  can release its own hand without over-releasing siblings. `Worker::cleanup`
  (the generation-guarded self-cleanup) dispatches
  `ToolExecutor::release_worker_hands` → `reclaim_hands(session_id,
  Some(worker_id))`, which reclaims only that scope. The VO holds no
  `ToolRouter`, so the release is a detached service call. Session teardown
  still reclaims any remaining coordinator/orphan hands via
  `reclaim_hands(session_id, None)`.
- **Sandboxes are refreshable, never the primary state source.** Durable agent
  state lives in the event log, artifacts, and object store, so a sandbox crash is
  recovered by marking the durable lease stale, claiming a new fenced
  `generation`, provisioning a fresh hand, and replaying the hash-validated
  `trusted_sandbox_manifest` to reinstall the prior files. No agent work product
  may live only in a sandbox.

Before the LLM call for a turn, the context pipeline selects relevant skills.
The selected trusted sandbox file references are copied into `ToolCallRequest`.
The root coordinator may read exact selected skill files directly from that
manifest without a hand; worker tool calls materialize the same files in the
worker hand even when the turn workflow and tool executor land on different pods.
The router still caches installed-file markers to avoid duplicate installs
inside one hand, but that cache is not the source of install intent. The model
only sees the manifest paths; full `SKILL.md` and supporting scripts remain
filesystem resources that are read or executed on demand.

Provider implementations must make cleanup best-effort and observable. Failed
cleanup should warn through `tracing`, not panic or hide the terminal session
outcome.

## Recovery

Hand providers classify failures into:

| Class | Meaning | Router action |
|---|---|---|
| Retryable | Transient provider or transport failure | Retry according to tool policy |
| ReProvision | Handle is stale or sandbox died | Destroy/recreate hand, then retry when safe |
| Fatal | Input, policy, or non-recoverable provider error | Return failure to the turn loop |

`health_check(handle)` lets the router replace dead sandboxes before a user
tool call discovers the failure.

Tool calls must also declare their idempotency behavior:

- `Idempotent`: safe to retry.
- `IdempotentWithKey`: safe when the remote API supports the idempotency key.
- `NonIdempotent`: retry only when no remote side effect was confirmed.

## MCP

MCP is the primary protocol for external integrations. Supported transports are
stdio, SSE, and streamable HTTP. Startup discovers tool definitions through
MCP, then the router exposes the selected tools exactly like built-ins and hand
tools. Stdio MCP launches a child process in the current pod and is allowed
only for local development; cloud startup rejects stdio MCP servers. Kubernetes
deployments must use HTTP/SSE MCP transports so any replica can handle a
request without depending on a pod-local process.

Credential handling is host-side:

1. The brain emits a normal tool call.
2. The MCP credential proxy resolves session-scoped access.
3. The proxy fetches real credentials from the configured vault.
4. The remote MCP request is enriched.
5. The result is returned with credentials stripped.

HTTP/SSE MCP servers get the strongest credential isolation because the proxy
can inject headers per request. Stdio MCP servers may still need startup
environment variables, so treat them as a weaker local-development-only
isolation boundary. The stdio pending-call map is only JSON-RPC response
demultiplexing state inside one transport and never session or request
correctness state.

## Security Rules

- Never place provider secrets in tool-call input or model-visible context.
- Prefer MCP or host-side helpers for external APIs instead of raw shell
  commands with secrets.
- Use parsed command normalization for shell action-policy patterns.
- Keep generated-code sandboxes ephemeral by default.
- Destroy or pause hands when sessions stop so stale credentials and state do
  not linger.

# 06 - Hands & MCP

_Hand providers, tool routing, MCP, sandbox lifecycle, and recovery._

## Contract

Hands are temporary execution environments. They are provisioned on first use
and destroyed when their owning scope reaches a terminal state. The root
coordinator runs sandbox-free; each conversational worker or sandbox-using
execution task owns an isolated hand, released when that scope becomes terminal.
Any remaining hands are released at session or run teardown. The brain never talks
to hands directly; it asks the `ToolRouter` to execute a named tool with
structured input.

Credentials must not be visible to generated code. Git, MCP, and external API
credentials are fetched or injected by trusted host-side code, not placed in
tool-call arguments.

## The Effective Sandbox Profile

Every provisioned hand carries exactly one `EffectiveSandboxProfile`, resolved
before the lease is claimed and honored or refused by the provider. It states
six dimensions, each required and typed: CPU millicores, memory mebibytes,
ephemeral disk mebibytes, an egress posture, an idle timeout, and a hard
maximum lifetime. Each resource and deadline is a nonzero bound or an explicit
`Unbounded`; there is no zero-means-unlimited, no `Option`, and no
`serde(default)`. Egress is `DenyAll`, `AllowList { destinations }`, or
`Unrestricted`. Revision identity belongs to the surrounding
`SandboxPolicySnapshot`; an allowlist does not carry a second revision.

Five layers contribute a `SandboxPolicySnapshot { revision, profile }`:

| Layer | Source | Unauthored revision |
|---|---|---|
| Deployment | `[sandbox_policy.deployment]` / `MOA_SANDBOX_POLICY_JSON` | `local-development-unbounded` (refused under `security_profile = cloud`) |
| Tenant | `moa.tenant_sandbox_policy` | `tenant-sandbox-unset` |
| Agent | `AgentDefinition.sandbox_policy`, pinned on the session | `agent-sandbox-unset` |
| Route | `[sandbox_policy.routes.<provider>]` | `route-sandbox-unset` |
| Origin | Effective session `CallOrigin` | `origin-production`, `origin-experiment-deny-all`, or `origin-generated-code-deny-all` |

Resolution is a restrictive intersection: the lowest bounded limit wins,
`Unbounded` is the identity element, `DenyAll` egress dominates, two allowlists
intersect, and an empty intersection becomes `DenyAll`. No layer can widen what
another bounded, which is why an unauthored layer contributes the identity
element rather than being absent — but it still contributes a *named* revision,
because all five revisions plus the serving provider's capability revision are
covered by the profile's stable SHA-256 identity hash. A layer that starts
declaring limits therefore changes the hash, and no sandbox provisioned under
the old identity can be reused.

Experiment trials and generated code contribute `DenyAll` egress at the origin
layer. A provider tier that cannot enforce deny-all — including direct host
execution — refuses admission before provisioning. Secure trials therefore run
on an enforcing tier rather than widening their origin policy to fit the host.

## Provider Capabilities

`HandProvider::capabilities()` is required and has no default body. It declares,
**per sandbox tier**, the resource ranges and granularity the provider can
enforce, the egress modes it can enforce, and who owns each deadline
(`Provider`, `DurableReaper`, or `None`). Admission compares the resolved
profile against this declaration and refuses before any lease claim and before
any provider API call. A bounded deadline whose owner is `DurableReaper` is
admissible only when that reaper is actually running.

Per-tier declarations exist because enforcement is a property of the tier: the
local provider can bound CPU and deny egress inside a Docker container and can
do neither for a bare host process.

| Provider / tier | Enforces | Refuses |
|---|---|---|
| Local, host tiers (`local`, `none`) | nothing; only `Unbounded` and `Unrestricted` | every bounded resource, every non-unrestricted egress |
| Local, `container` | `--cpus`, `--memory`, `--network none` / `bridge` | bounded ephemeral disk (`--storage-opt size=` is a no-op on overlay2), egress allowlists (no per-destination filter exists) |
| E2B, `microvm` | `timeout` (hard lifetime), `allow_internet_access` | bounded CPU/memory/disk (template-fixed), egress allowlists, an *unbounded* hard lifetime (E2B has no "no timeout" value) |
| Daytona, `container` / `none` | `autoStopInterval` (idle, whole minutes) | bounded CPU/memory/disk, non-unrestricted egress, a non-whole-minute idle timeout |

Refusals are the point of the table. A provider that accepted a dimension and
dropped it would turn policy into decoration.

## Provider Map

| Provider | Use | Notes |
|---|---|---|
| Local | Zero-setup tests and development | Uses a workspace directory and optional Docker support. |
| Docker | Local/containerized execution | Hardened by `moa-hands` and `moa-security` policies. |
| Daytona | Cloud workspace provider | Compiled into the normal build; enabled by runtime cloud-hands config. Supports pause/resume/destroy around idle sessions. |
| E2B | MicroVM isolation | Compiled into the normal build; enabled by runtime cloud-hands config. Use for untrusted or security-sensitive execution. |
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

Cloud hand routing is runtime-configured, not feature-gated. Set
`cloud.hands.default_provider` (or `MOA_CLOUD_HANDS_DEFAULT_PROVIDER`) to the
first cloud provider, then set `cloud.hands.fallback_providers` (or
`MOA_CLOUD_HANDS_FALLBACK_PROVIDERS`) to an ordered comma-separated fallback
list such as `e2b`. The router tries the next provider when provisioning or
health-check fails before a tool runs. After a tool has started, it only moves
to a fallback provider for tools declared `Idempotent`; non-idempotent tools
still return an error instead of risking duplicate side effects. Once a fallback
provider succeeds for a session or worker scope, that scope uses the successful
provider first until the hand scope is reclaimed.

`ActionEnvelope` is the durable policy-facing record for one tool invocation.
It includes the review id, tenant, user, session or worker origin, tool
call id, tool name, normalized input, input summary, risk level, action class,
optional execution-run/task and artifact origin metadata, idempotency key, and creation time.
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

The registry's default loadout is an **ordered** list — built-ins, then the
sandbox descriptors in their authored order, then discovered connector tools in
catalog order. That order is the deployment's declared capability priority. When
a loadout exceeds the per-turn schema cap, the context pipeline reduces along
that order after first keeping the loop's control tools and any tool the pinned
agent or its skills explicitly declared; it canonicalizes by name only *after*
selecting, so the cached prompt prefix stays byte-stable. Reducing by name
instead would drop tools for how they are spelled, which says nothing about
whether the turn needs them.

The execution capability catalog is the planner/compiler source of truth over
these governed operations. Each entry has a stable reference and version,
governed runtime-contract revision, description, input/output schemas,
action/risk and idempotency classes, execution class (`data`, `compute`,
`model`, or `external`), source provenance, authorization metadata, and optional integer cost estimate. It includes typed
built-ins, actions, skill actions/code, memory operations, connected MCP tools
whose schemas and policies are stable, and datasource reads only when a typed
query operation exists. A connection identifier alone is not executable.

`Capability` and bounded `Agent` execution resolve through the same action
policy and `ToolExecutor`/typed-service owners as root tool calls. The graph
interpreter never calls a hand, MCP server, datasource, or memory store directly.
Capability availability and authorization restrict what may run; resource
budgets restrict only how much may run.

`ToolCallRequest.resource_budget` is the downward-only runtime slice admitted
for that call. `ToolExecutor` preserves it across durable retries and invokes
the router's budget-aware recovery path; the router refuses an exhausted
tool-call allowance, checks the deadline before provisioning, and applies the
same bound to local, remote-sandbox, and retry execution. Ordinary calls state
`Unbounded` explicitly. Experiment Session turns carry their reserved target
slice here rather than relying on parent-side reconciliation after the tool has
already run.

The router publishes the executable registry and its model-visible schemas as
one immutable snapshot. A refresh cannot expose a new executor with stale
prompt schemas, or the reverse. Every conversational prompt and durable
execution capability pins each admitted tool's complete governed-contract
revision. Policy evaluation and dispatch refuse that tool when the live
revision no longer matches, including schema, policy, retry, output, ownership,
annotation, and routing changes.

## Lifecycle

Active hands are keyed by owning session/run scope and provider. Conversational
worker leases use `(session_id, worker_id, provider)`; execution-task leases use
the stable run/task origin and generation. The authoritative binding lives in
Postgres; `ToolRouter` process maps are reconnect caches only. A
first tool call claims a durable lease before provisioning the hand. Later tool
calls on any Kubernetes replica load the lease, reconnect or resume the provider
handle when healthy, or mark it stale and reprovision with a new generation. On
terminal session status, cancellation, failure, or panic cleanup, the
orchestrator calls `reclaim_hands(session_id, None)`, which lists durable
leases for every worker scope rather than only handles cached in the current
process.

A lease persists the exact profile, its identity hash, all five source
revisions, the capability revision, a renewable `idle_expires_at`, and an
immutable `hard_expires_at`. `NULL` on either deadline means that dimension was
explicitly `Unbounded`. Renewal moves only the idle deadline and is capped at
the hard deadline (`LEAST(requested, hard_expires_at)`), and a lease already
past its hard deadline cannot be renewed at all — so a continuously busy sandbox
still dies on schedule. Reuse and recovery recompute today's policy and compare
identity hashes; any mismatch fences the lease stale and reprovisions.
Provisioning uses the same resolved profile that was persisted on the claim.
When replacing a stale binding, the claimant destroys and clears the old
durable handle before provisioning a replacement.

### The durable reaper

A hard maximum lifetime is only policy if something destroys the sandbox when it
fires, and the sandboxes that most need destroying belong to sessions that will
never send another request. `HandLeaseReaper` is that owner. It is started by
`runtime::jobs::start_hand_lease_reaper` before the orchestrator accepts
traffic — startup fails outright if no hand provider is registered — and sweeps
independently of traffic. It claims bounded batches with `FOR UPDATE ... SKIP
LOCKED` so competing replicas take disjoint work. Each claim has a UUID owner
token and expiry, so another replica can reclaim it after a crash. Destruction
runs with bounded concurrency (four by default); finalize and retry updates
must match both the claimed generation and owner token. A failed destroy
releases the generation back to `stale` behind exponential backoff, never to
`active`: a sandbox the reaper decided to destroy is not one anyone should get
back.

### Isolated Sandbox Ownership

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

This Worker model remains for conversational delegation in `act`; Worker is not
an execution-plan node or bulk DAG primitive. A sandbox-using `ExecutionTask`
gets the same isolation and generation-fenced recovery under its task identity.
Dynamic map execution has no application hand/worker fan-out cap: every stable
logical item is submitted after atomic budget reservation, while Restate
concurrency and provider pacing determine how many physical sandboxes run at
once.

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
tool call discovers the failure. When multiple cloud hand routes are configured,
the router prefers provider fallback before same-provider reprovision for these
pre-execution failures.

Tool calls must also declare their idempotency behavior:

- `Idempotent`: safe to retry.
- `IdempotentWithKey`: safe when the remote API supports the idempotency key.
- `NonIdempotent`: retry only when no remote side effect was confirmed.

## MCP

MCP is the primary protocol for external integrations. Supported transports are
SSE and streamable HTTP. Startup discovers tool definitions through MCP, then
the router exposes the selected tools exactly like built-ins and hand tools.
MCP servers must be reachable over HTTP/SSE so any Kubernetes replica can handle
a request without depending on a pod-local process.

### Server-qualified tool references

A discovered connector tool registers under
`mcp__{server_byte_len}_{server}__{remote_tool}` — not under the name the server
publishes. Including the server byte length makes the encoding injective even
when server and tool names contain separators. Duplicate qualified insertion is
rejected. That qualified reference is the tool's identity
everywhere on MOA's side of the connector boundary: the model-visible schema,
the registry key, action-policy rules, the persisted `ToolCall` event, and the
execution capability catalog. Only the outbound `tools/call` and a tenant
connection binding's `allowed_operations` use the server's own name, because
those are the two places the connector's vocabulary is the correct one.

Qualification exists so one connector cannot affect another. Before it, a server
publishing a tool called `bash` failed router construction outright, taking down
every unrelated tool in the deployment; two servers publishing the same tool
name could not coexist at all.

**Operator-visible consequences.** Both bite on upgrade, and neither is cosmetic:

- Any persisted action-policy rule or `permissions.*` pattern that targets a
  connector tool by its unqualified name stops matching. For an
  `admin_review` pattern this fails **open** — a tool that was review-gated
  becomes ungated. The router reports every configured permission pattern that
  matches no registered tool at startup (and after each catalog refresh), so a
  pattern left behind by this rename is visible rather than silent, but the
  patterns still have to be rewritten. One `mcp__*` pattern now gates every
  connector tool regardless of what any server names its tools, which no
  pattern could express before.
- Model-visible tool names change, so the cached prompt prefix changes for any
  deployment running MCP servers. Expect one cache-cold period per session
  after upgrade.

### Connector health and catalog refresh

Each configured server is `required` or optional (the default), and either
`eager` or `lazy` for discovery. An optional server that fails discovery removes
only its own tools and is recorded as typed health; a required one that fails
discovery is a startup failure carrying that health, because a deployment that
silently drops a required integration is indistinguishable from one that never
configured it. `required` plus `lazy` is rejected at startup: "required" means
verified at startup, and a lazily discovered server has not been contacted.

Health is per connector — `Pending`, `Ready`, `Degraded`, `Unavailable` — never
an aggregate, because an aggregate cannot express "this optional integration is
down and every other tool is fine", which is the state the router has to serve.
A background refresh re-discovers every connector on an interval. A connector
that fails a refresh after a previous success keeps serving its last-known-good
tools and reports `Degraded`, so one transient error cannot silently shrink the
model's loadout. The catalog is published as a whole snapshot, so no prompt
compilation, capability listing, or dispatch ever observes a half-refreshed
connector.

Each registered tool carries a governed-contract revision. The revision is
recorded on compiled capabilities and paired atomically with the exact tool
schemas offered in conversational prompts. A changed or withdrawn contract is
therefore rejected before policy evaluation and checked again against the
immutable snapshot used for dispatch, including across rolling deployments.

### Credentials

An MCP server may name one deployment environment variable using `bearer`,
`oauth`, or `api_key`. The router reads it once at construction and fails
startup if it is missing or empty. Omitting `credentials` configures an
unauthenticated connector.

Credential handling is host-side:

1. The brain emits a normal tool call.
2. Data-class egress governance runs before invocation.
3. The same sensitive authentication headers are applied to initialize,
   initialized notification, discovery, and `tools/call`.
4. The result is returned with credentials stripped.

HTTP/SSE MCP servers get host-side credential isolation. The MCP client does not
launch local subprocesses or
store credential-bearing environment variables for server startup.

## Security Rules

- Never place provider secrets in tool-call input or model-visible context.
- Prefer MCP or host-side helpers for external APIs instead of raw shell
  commands with secrets.
- Use parsed command normalization for shell action-policy patterns.
- Keep generated-code sandboxes ephemeral by default.
- Destroy or pause hands when sessions stop so stale credentials and state do
  not linger.

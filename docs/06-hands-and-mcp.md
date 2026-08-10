# 06 - Hands & MCP

_Hand providers, tool routing, MCP, sandbox lifecycle, and recovery._

## Contract

Hands are ephemeral execution environments. `SandboxWorkspace` is the separate,
durable, tenant-owned filesystem aggregate. Compute can be destroyed while its
workspace is retained, and a workspace can be purged without treating a running
hand as its source of truth. The root coordinator runs sandbox-free; each
conversational worker or sandbox-using execution task may own one typed
workspace and attach one isolated hand under independent writer and instance
fences. The brain never talks to either directly; it asks the `ToolRouter` to
execute a named tool with structured input.

The complete data model, state machine, commit barrier, provider storage
capabilities, provider-pinning rule, and purge order live in
[Sandbox Workspaces](25-sandbox-workspaces.md). Restate/session durability does
not make arbitrary files in a hand durable. Only a verified portable checkpoint
is the committed filesystem revision.

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

Persistent-workspace admission uses an operator-authored provider-account route,
the workspace's durability class, the hand's effective sandbox profile, and a
durable capacity reservation. `SandboxStorageProvider` exposes one required
contract for every admitted route: prepare and attach mutable storage, publish
and restore portable checkpoints, delete, enumerate, and reconcile. After
materialization, a workspace remains provider-pinned unless a verified portable
checkpoint exists and the target satisfies the same format and security
profile.

## Provider Map

| Provider | Use | Notes |
|---|---|---|
| Local | Zero-setup tests and development | Uses isolated per-instance scratch plus durable portable checkpoints; host-local execution is refused for cloud/multi-tenant profiles. |
| Docker | Local/containerized execution | Hardened by `moa-hands` and `moa-security` policies. |
| Daytona | Cloud workspace provider | Uses one tenant-dedicated volume per provider-account/isolation cell with opaque per-workspace subpaths; portable checkpoints remain recovery authority. |
| E2B | MicroVM isolation | Uses GET-verified running compute only, exports the reserved mutable root into a portable checkpoint, kills the source, and restores into fresh compute. Pause/resume, provider snapshots, and E2B volumes are absent from the persistence route because the public pause and snapshot contracts preserve process memory. |
| Operator MCP | Deployment-wide external tools and SaaS integrations | Routed through the process-wide `MCPClient` using operator configuration. |
| Tenant connector | Reviewed tenant HTTP actions | Routed through exact connection/binding/generation pins and the shared governed tool path. |

Sandbox providers implement the `HandProvider` trait from `moa-core`.
Operator MCP and tenant HTTP connectors are separate governed tool backends;
tool routing depends on their typed boundaries rather than provider-specific
clients.

### Provisioning operation identity

Every `HandSpec` requires a typed `HandProvisioningOperationId` that the
durable lease creates and records before the first provider API or local-runtime
call. The same claim records an absolute provisioning deadline and a
`reap_not_before` time strictly after that deadline plus the provider-visibility
grace. Platform-created specs carry the absolute deadline through
`budget.deadline`, and the platform bounds the complete provider create future
by it. A provider may enforce a shorter internal timeout but may never widen the
durable deadline. `HandProvider::provision` is idempotent for that operation identity and
the same creation-relevant spec: a replay resolves a resource already carrying
the identity when possible, while reuse with a different spec or profile
identity fails closed. Providers attach the identity as part of resource
creation, never as a second mutation after creation.

`HandProvider::provisioned_hands` is required with no default body and returns
every live resource carrying an operation identity. The return type is a list,
not an optional single handle, because provider APIs without an atomic
idempotency key can expose duplicates after an ambiguous create. Enumeration
includes recoverable non-running resources such as paused hands and follows any
provider pagination. The durable reaper cannot claim an ambiguous create before
its persisted reconciliation time. It destroys every returned handle, confirms
an empty observation, waits the explicit confirmation interval while renewing
the exact reaper claim, and confirms emptiness again before finalization.

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
   - operator MCP client or installed tenant HTTP connector runtime.
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
list such as `e2b`. Before workspace materialization, the router may try the next
provider when capability/capacity admission, provisioning, or health-check
fails before a tool runs. Once filesystem state exists, idempotency alone is
insufficient: the workspace is provider-pinned. Stateful fallback requires a
verified portable checkpoint and a target provider that admits its checkpoint
format and security profile. Otherwise routing fails closed without starting an
empty replacement. An ambiguous command, quiesce, or commit never falls back.

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

Tools come from four sources:

| Source | Examples | Execution |
|---|---|---|
| Built-ins | memory tools, search helpers | In-process Rust handlers |
| Hand tools | `bash`, `file_read`, `file_write`, `file_search` | Local/Docker/Daytona/E2B hand |
| Operator MCP tools | GitHub, browser, database, SaaS tools | Deployment-configured MCP transport |
| Tenant connector actions | Reviewed constrained HTTP operations | Exact installed connector binding |

Tool descriptors include name, schema, execution backend, risk level, action
class, default action-policy effect, and output budget. The context pipeline
injects only the currently active subset to protect prompt budget and cache
stability.

The registry's default loadout is an **ordered** list — built-ins, then the
sandbox descriptors in their authored order, then operator MCP tools in catalog
order. An authorized request may add a separate ephemeral tenant connector
overlay from exact agent bindings. That order is the declared capability priority. When
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
built-ins, actions, skill actions/code, memory operations, operator MCP tools,
authorized exact-generation tenant connector actions, and datasource reads only when a typed
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

Active hands are keyed by typed worker or execution-task scope and provider.
The durable `SandboxWorkspace` is keyed independently from its current compute
lease. The authoritative bindings live in Postgres; `ToolRouter` process maps
are reconnect caches only. First use resolves or creates the tenant-scoped
workspace, claims its writer, restores or attaches the committed state, and only
then provisions compute. Later calls on any Kubernetes replica verify both
workspace fences and the hand lease before reconnecting or replacing compute.

The architecture and runtime admit no bare-session/coordinator workspace.
Sandbox dispatch requires a typed worker or execution-task workspace scope and
rejects its absence before workspace reads or provider I/O. Session-wide
terminal cleanup is only an aggregate teardown selector over typed compute
attachments; it is not an ownable hand or workspace scope. Terminal cleanup
checkpoints according to policy, releases compute, and leaves retained workspace
state or reconciliation ownership intact.

The provisioning claim atomically records a fresh operation ID, absolute create
deadline, and later reconciliation time with its lease generation before
calling the provider. A successfully recorded handle retains
the operation ID that created it. If a process exits after provider creation but
before handle activation, the lease therefore still names the external
operation; the reaper enumerates and destroys every matching provider resource
without relying on process memory or a recorded handle. A replacement
generation receives a different operation ID, so an older resource remains
independently discoverable and cannot be mistaken for the replacement.

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

### The durable compute reaper

A hard maximum lifetime is only policy if something destroys the sandbox when it
fires, and the sandboxes that most need destroying belong to worker or
execution-task owners that may never send another request. `HandLeaseReaper` is
that owner. It is started by
`runtime::jobs::start_hand_lease_reaper` before the orchestrator accepts
traffic — startup fails outright if no hand provider is registered — and sweeps
independently of traffic. It claims bounded batches with `FOR UPDATE ... SKIP
LOCKED` so competing replicas take disjoint work. A sweep claims no more rows
than its destruction concurrency, so every claimed row begins polling and
heartbeating immediately. Each claim has a UUID owner token and expiry, so
another replica can reclaim it after a crash. Long provider reconciliation
renews that exact generation/operation/handle/token claim before it expires.
Destruction runs with bounded concurrency (four by default); finalize and retry updates
must match both the claimed generation and owner token. A failed destroy
leaves the generation reaper-owned as `failed` behind exponential backoff,
never `stale` or `active`: a sandbox or unresolved provisioning operation the
reaper decided to destroy is not one request traffic should get back. Claims
include abandoned provisioning rows whose handle is still null; the reaper
resolves both the current operation ID and any operation ID retained by a prior
stored handle, deduplicates all discovered handles, and finalizes only after
every resource has been reconciled and two empty provider observations are
separated by the explicit confirmation interval. V57 also rejects generation
rotation unless the operation ID and provisioning deadline rotate with it, so
an already-running pre-V57 writer fails before creating an uncorrelated hand.

This reaper owns ephemeral compute only. It may stop or destroy a hand;
it cannot delete a retained workspace, advance or discard its checkpoint head,
or release a newer workspace attachment. Workspace reconciliation and retention
use their own generation-fenced operation ledger and cleanup owner.

### Workspace And Compute Ownership

Workspace ownership is typed as `Worker { session_id, worker_id }` or
`ExecutionTask { run_id, task_id }`. There is no session-only variant. Each
workspace has at most one writable attachment, enforced by a database
constraint/compare-and-set over `writer_epoch`; `instance_generation` separately
fences provider compute. Siblings never share a writable workspace or hand:

- **The coordinator is sandbox-free.** Root-turn preparation in `brain_bridge.rs`
  filters the coordinator's tool schemas through `ToolRouter::tool_requires_sandbox`
  (true only for `ToolExecution::Hand` tools) and hard-excludes those tools except
  manifest-backed selected-skill `file_read`. The root `file_read` path is served
  directly from the trusted manifest by `ToolExecutor`; it does not provision a
  hand. The worker tool subsets keep the hand tools, so all real computation is
  delegated. Zero workers means zero sandboxes.
- **Each valid owner has one workspace and one writable hand attachment.** A
  worker or sandbox-using execution task carries its typed workspace binding
  through the tool executor. N parallel owners have N independent workspace
  scopes and compute instances.
- **Release and retention are independent.** `Worker::cleanup` and execution
  task cleanup checkpoint according to policy and release only their current
  compute attachment. They do not delete retained workspace state or drop an
  ambiguous-operation ledger. Workspace deletion is a separately authorized,
  generation-fenced lifecycle.
- **Working sandboxes are replaceable, never the committed source.** A sandbox
  crash fences its instance, provisions fresh compute, restores the verified
  current portable checkpoint, then reinstalls the current trusted manifest and
  runtime controls outside the checkpoint root. Event history and a trusted-file
  manifest cannot reconstruct arbitrary task-created files; those files survive
  only when the checkpoint commit barrier published them.

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

Every sandbox tool has an exhaustive `WorkspaceEffect::ReadOnly` or
`WorkspaceEffect::MayWrite` declaration. Read-only calls do not advance the
workspace. A `MayWrite` call persists intent, executes under a cancellable
deadline, quiesces and proves its writer stopped, flushes and verifies provider
ownership, streams the filesystem-only mutable root into a bounded encrypted
portable checkpoint, validates its manifest/chunks, compare-and-set advances
the workspace head, and only then persists a successful `ToolResult`. An
ambiguous stage leaves the workspace dirty and `reconciling`, returns
non-success, retains cleanup ownership, and forbids blank fallback. The complete
state machine and seven-step barrier are canonical in
[Sandbox Workspaces](25-sandbox-workspaces.md).

## Recovery

Hand providers classify failures into:

| Class | Meaning | Router action |
|---|---|---|
| Retryable | Transient provider or transport failure | Retry according to tool policy |
| ReProvision | Handle is stale or sandbox died | Destroy/recreate hand, then retry when safe |
| Fatal | Input, policy, or non-recoverable provider error | Return failure to the turn loop |

`health_check(handle)` lets the router replace dead sandboxes before a user
tool call discovers the failure. Before workspace materialization, multiple
cloud routes may use typed capability/capacity fallback. After materialization,
a fresh hand restores the provider-pinned workspace; cross-provider fallback is
allowed only from a verified portable checkpoint compatible with the target.

Tool calls must also declare their idempotency behavior:

- `Idempotent`: safe to retry.
- `IdempotentWithKey`: safe when the remote API supports the idempotency key.
- `NonIdempotent`: retry only when no remote side effect was confirmed.

## MCP

MOA has two distinct MCP surfaces. The inbound `/mcp` protected resource is an
edge API adapter into existing product services and never supplies agent tools.
Operator-owned deployment MCP is the only outbound MCP tool surface. Tenant
connector connections support reviewed constrained HTTP actions, not MCP.

### Operator-owned deployment MCP

`OperatorOwnedMcp` is process-wide deployment configuration. Supported
transports are SSE and Streamable HTTP. Startup or a background refresh
discovers tools, then the router exposes the selected immutable catalog exactly
like built-ins and hand tools. Servers must be remotely reachable so any
Kubernetes replica can handle a request without a pod-local process.

A discovered operator tool registers as
`mcp__{server_byte_len}_{server}__{remote_tool}`. This injective qualified name
is its model-visible schema name, registry key, action-policy key, persisted
`ToolCall` name, and execution-catalog identity. Only outbound `tools/call`
uses the server's remote name. Duplicate qualified insertion is rejected, so a
server cannot collide with a built-in or another server.

Operator policy rules and `permissions.*` patterns must use the qualified
reference. A stale unqualified pattern no longer matches; `mcp__*` gates every
operator MCP tool. Changing model-visible names also changes the cached prompt
prefix for sessions that receive those tools.

Each configured server is `required` or optional, and `eager` or `lazy`.
Required plus lazy is rejected because required means verified at startup. An
optional discovery failure affects only that server. After a prior success, a
transient refresh failure retains its last-known-good tools and reports
`Degraded`; publication swaps the complete catalog snapshot atomically.

An operator server may name one deployment environment variable using
`bearer` or `api_key`, or omit credentials. The router fails startup when named
material is missing. It marks headers sensitive and applies the same
credential to initialize, initialized notification, discovery, and
`tools/call`. Outbound operator MCP OAuth is not supported by this config.


Tenant action tool names use deterministic `conn__...` lookup references, but
the runtime never parses a name for authority. It dispatches only through the
persisted connection/binding/generation/definition/contract pin supplied by the
scoped catalog.

See [Connectors And Connections](24-connectors-and-connections.md) for the
connection lifecycle, credential vault, management API, and rollout contract.

## Security Rules

- Never place provider secrets in tool-call input or model-visible context.
- Prefer MCP or host-side helpers for external APIs instead of raw shell
  commands with secrets.
- Use parsed command normalization for shell action-policy patterns.
- Keep generated-code compute ephemeral; persist only the filesystem-only
  mutable root through the governed `SandboxWorkspace` commit barrier.
- Destroy hands when their worker/execution-task scope
  stops so stale credentials and processes do not linger; retain or delete the
  workspace only through its independent policy and purge lifecycle.

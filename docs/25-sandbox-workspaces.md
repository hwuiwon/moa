# 25 - Sandbox Workspaces

_Durable tenant filesystem state over ephemeral sandbox compute._

## Purpose And Status

This document is the canonical architecture contract for persistent sandbox
workspaces. It deliberately separates two lifecycles:

- a **hand** is an ephemeral provider compute instance that can be stopped,
  destroyed, replaced, or reaped; and
- a **`SandboxWorkspace`** is durable tenant-owned filesystem state with its own
  identity, retention, checkpoint history, deletion fence, and cleanup owner.

Compute teardown never implies workspace deletion. Workspace retention never
requires retaining compute. Restate makes the lifecycle orchestration replayable;
it does not make arbitrary files inside a sandbox durable. Session events and
Restate journals may reference workspace revisions, but provider storage and the
portable checkpoint object store own the bytes.

This is a breaking contract. Runtime admission accepts only worker-owned and
execution-task-owned workspaces; sandbox dispatch without one of those typed
owners fails before workspace reads or provider I/O.

## Scope

Persistent workspaces retain filesystem data required by sandbox tools across
compute replacement and process failure. They do not schedule long-horizon work,
retain process memory by default, provide multi-writer collaboration, share
workspaces across tenants, or make databases and block-storage workloads safe on
provider FUSE volumes.

The default retained surface is **filesystem-only**. Only the reserved mutable
tenant-data root is checkpointed. Processes, RAM, network connections, runtime
control files, trusted skill files, credentials, tokens, authorization, policy,
and provider configuration are outside that root and are reconstructed from
current authority on every provision, restore, or resume.

## Ownership Boundaries

| Boundary | Owner | Contract |
|---|---|---|
| Shared IDs, scopes, lifecycle types, capabilities, bindings, and traits | `moa-core` | Provider-neutral types only; no provider or persistence implementation. |
| Workspace lifecycle, tenant-scoped repositories, operation/capacity ledgers, checkpoint/archive logic, reconciliation, and local/Daytona/E2B adapters | `moa-hands` | Owns workspace domain behavior and every provider storage operation. |
| Authentication/authorization, Restate `ctx.run` boundaries, service/workflow calls, runtime composition, and durable workflow sequencing | `moa-orchestrator` | Authorizes before protected reads, derives tenant/scope from verified durable state, and journals calls without owning filesystem bytes. |
| Workspace rows and immutable revision metadata | Postgres | Product-visible ownership, lifecycle, fences, operation intent, grants, and capacity truth. |
| Active mutable bytes | Selected sandbox/storage provider | An accelerator or working copy, never the committed revision. |
| Portable checkpoint bytes | Durable S3-compatible object storage | Provider-independent committed recovery authority; RustFS is the local development binding. |
| Wrapped checkpoint keys | Durable KMS | Key lifecycle is independent of compute and object metadata. |

Neither a provider resource identifier nor a local path is an authorization
token. They are opaque implementation references and never enter public DTOs.

## Identity And Scope

Every workspace belongs to exactly one immutable `tenant_id` and exactly one
typed execution scope:

```text
SandboxWorkspaceScope =
  Worker { session_id, worker_id }
  | ExecutionTask { run_id, task_id }
```

There is no coordinator or bare-session workspace scope. Coordinator sandbox
work is delegated to a conversational worker or represented by a durable
execution task with an explicit owner. Tenant identity comes from verified
session/run/task state, never request bodies, model output, tool arguments,
provider labels, provider IDs, or mount paths.

The important identities are independent:

- `workspace_id` names durable logical filesystem state;
- `writer_epoch` fences the one logical writable attachment;
- `instance_generation` fences one provider compute instance;
- the hand-lease generation fences ephemeral compute lifecycle; and
- `checkpoint_id` plus workspace generation names one immutable committed
  revision.

No generation is reused as another generation's substitute.

## Durable Data Model

The canonical Postgres model is:

| Row family | Required responsibility |
|---|---|
| `moa.sandbox_workspaces` | Immutable tenant and typed scope, provider binding, durability class, lifecycle state, writer epoch, committed checkpoint head/generation, retention deadline, delete fence, timestamps. |
| `moa.sandbox_workspace_checkpoints` | Immutable parent-linked revisions, source writer/instance generation, content kind, provider/object reference, manifest digest, logical bytes, operation, lifecycle and retention state. |
| `moa.sandbox_workspace_operations` | Create/attach/commit/checkpoint/restore/delete intent, canonical request hash, expected generations, deadline, `not_sent | unknown | confirmed` outcome, claim token/expiry, attempts, backoff, safe error code, reconciliation time. |
| `moa.sandbox_workspace_grants` | Desired OpenFGA owner/use grants and generation-fenced inverse tuple intent. |
| `moa.sandbox_provider_accounts` | Non-secret deployment/provider/isolation-cell identity and generation, organization/project fingerprint, configured limits, observed inventory, headroom, health. |
| `moa.sandbox_storage_resources` | Tenant-owned external storage, such as a Daytona tenant volume, with account ownership, provider reference, generation, deletion intent, and verified ownership metadata. |
| `moa.sandbox_capacity_reservations` | Pending/committed capacity by tenant, provider account, operation, and exact resource kind: `workspaces`, `volumes`, `checkpoints`, or `logical_bytes`. |
| `moa.hand_leases` workspace fields | `workspace_id`, workspace writer epoch, workspace instance generation, and restored checkpoint ID; the lease still owns only compute. |

Every relationship that crosses a tenant-owned table includes `tenant_id` in
its foreign key. Tenant-facing repositories use `ScopedConn`, forced RLS,
tenant-first predicates, and immutable tenant ownership. Cross-tenant
reconciliation uses a separate, narrow maintenance path unavailable to request
handlers.

## Workspace State Machine

```text
creating -> ready -> restoring -> active -> quiescing -> committing -> active
                                                       |
                                                       +-> reconciling -> active|failed

ready|active|failed -> deleting -> deleted
```

State meanings are strict:

- only `active` under the current writer and instance fences accepts dispatch;
- `quiescing` blocks new dispatch and waits for, cancels, or fences every
  in-flight command;
- `committing` publishes only by compare-and-set against the prior committed
  generation and current writer epoch;
- `reconciling` retains operation intent, reservation, working state, and
  cleanup ownership while an external outcome is ambiguous;
- `failed` retains the prior committed checkpoint and every ledger needed for
  cleanup; dirty provider state is never promoted; and
- `deleting` immediately fences local access, even if a short-lived OpenFGA
  positive cache still contains an allow.

There is no transition from a missing or ambiguous state to an empty writable
workspace. Recovery either proves the current working state safe or restores a
verified committed checkpoint.

## One Writer And Fencing

A workspace has at most one writable attachment. The rule is enforced with a
database constraint/compare-and-set, not a process mutex, sticky routing, or a
Restate key alone.

Every dispatch and provider callback verifies the workspace, tenant,
provider-account generation, local lifecycle access epoch, `writer_epoch`,
`instance_generation`, current checkpoint revision, policy hash, credential
generation, trusted-manifest revision, runtime profile revision, and deadline.
A stale actor may leave an external orphan for reconciliation, but cannot
dispatch, publish a checkpoint, advance the head, delete a newer resource, or
release another operation's reservation.

An external create, attach, commit, checkpoint, restore, or delete is
preceded by a durable operation intent containing its request hash, expected
generations, reservation, and deadline. An unknown outcome remains reserved and
reconciling; it is never blindly retried or treated as absence.

Ambiguous commit/checkpoint recovery reconstructs the exact persisted binding,
creating checkpoint, attached lease generation, provider account generation,
and deterministic portable-checkpoint reference. It calls only provider
reconciliation, requires a complete verified publication plus compute
disposition, and atomically advances checkpoint/head/lease/operation/reservation
under the live reaper claim. It never resends commit or merely marks a resource
present. Reconciliation claims use their own 60-second default lease, separate
from longer checkpoint-GC claims, so a crashed replica is reclaimable promptly
without allowing an expired claimant to finalize.

## Commit Barrier

Every sandbox tool descriptor declares `WorkspaceEffect::ReadOnly` or
`WorkspaceEffect::MayWrite`. Read-only tools do not advance workspace state.
A mutating call follows this barrier:

1. Authorize and verify tenant, owner, provider-account generation, all writer
   and instance fences, current policy/credential/trusted/runtime revisions, and
   the lease deadline.
2. Execute the command under a provider-enforced or separately cancellable
   deadline in one named Restate run, classify its output, and journal the
   secured output plus a typed `workspace_commit_required` receipt. This run
   contains no checkpoint publication.
3. In a second named Restate run, persist the deterministic commit intent and
   checkpoint row, then enter `quiescing` and `committing`. Replay resumes this
   step from the exact tool-call, writer, instance, account, and parent-head
   fences without dispatching the command again.
4. Quiesce the workspace and prove that the remote writer stopped; an HTTP
   response alone is insufficient.
5. Flush and re-verify the same account, mount, and owner metadata, then stream
   only the mutable tenant-data root into a new portable checkpoint.
6. Validate the complete canonical manifest and chunk digests, then
   compare-and-set the immutable checkpoint ID and current generation.
7. Persist a successful `ToolResult` only after step 6 succeeds.

The commit step owns a fresh bounded recovery deadline; caller cancellation or
an expired command deadline cannot strand an already-journaled mutation. A
provably `not_sent` commit may renew its expired provider deadline by exact CAS.
Immediately before provider I/O the operation becomes `unknown`, so a crash
during or after the request can never renew or resend it blindly. Catalog or
policy drift after the command journaled also cannot block publication: the
typed receipt is the authorization proof, while provider/workspace identities
are reloaded from durable tenant-scoped state rather than accepted from the
caller.

If command termination or checkpoint publication is ambiguous, the router
returns a non-success result, marks the working copy dirty, moves the workspace
to `reconciling`, retains cleanup ownership, and performs no provider fallback.
The prior committed checkpoint remains authoritative and must be restored
before another dispatch.

Portable checkpoint format v1 is a canonical, sorted manifest plus bounded
zstd-compressed chunks. Chunks are independently authenticated and encrypted;
their AAD binds tenant, workspace, checkpoint, chunk index, digest, and format
version. Upload, verification, download, decompression, and extraction stream
with bounded memory. The format accepts only normalized UTF-8 relative paths,
regular files, directories, and safe relative symlinks. It rejects absolute or
escaping links, hard links, devices, FIFOs, sockets, excessive depth/count/size,
and decompression expansion beyond configured limits. Object keys are opaque,
writes are create-only, and restore validates into a fresh root before atomic
promotion.

## Provider Binding And Admission

Provider selection uses an operator-authored provider-account route rather than
runtime capability negotiation. Every admitted storage provider implements the
same required mutable-filesystem and portable-checkpoint contract. Admission
requires the configured durability class, a compatible hand sandbox profile,
deadline/cancellation enforcement, and sufficient durable reserved capacity
before provider I/O. Providers may additionally refuse creation when a live
account check, such as Daytona volume headroom, fails.

After workspace materialization, the workspace is provider-pinned. Stateful
fallback is allowed only when the source has a verified portable checkpoint and
the target route satisfies its format and required security profile. Otherwise
fallback fails closed and the workspace remains pinned.
No router may start an empty fallback for stateful work.

Provider bindings are:

| Provider | Active working state | Committed recovery | Default exclusions |
|---|---|---|---|
| Daytona | One tenant-dedicated volume per selected provider-account/isolation cell, with one opaque provider-enforced subpath per workspace and one writable mount | Portable checkpoint after quiesced flush | No per-session/workspace volumes, cross-tenant volume pools, whole-volume mounts, concurrent writers, or provider-native snapshot authority |
| E2B | Fresh running sandbox with auto-pause and auto-resume disabled; commit exports the reserved data root and then kills compute | Portable checkpoint before a mutating result becomes durable; restore always targets a newly provisioned hand | No pause/resume or provider snapshots because both preserve process memory, and no production E2B volumes while their durability contract remains unsuitable for MOA's correctness boundary |
| Local/Docker | Per-instance isolated scratch/bind directory | Portable checkpoint in durable S3-compatible storage | No multi-tenant host-local execution, orchestrator-mounted cross-tenant RWX tree, or Docker-commit backup |

Only mutable filesystem storage and portable checkpoints are part of the
storage-provider contract. Paused sandboxes and provider-native snapshots are
not modeled as workspace state or committed revisions.

## Retention, Reconciliation, And Purge

Compute reaping and workspace retention are independent jobs. Reaping a hand
may stop or destroy compute, but cannot delete a retained workspace or
its checkpoint head. Workspace deletion is generation-fenced and cannot race a
newer attachment.

Checkpoint retention is one validated replica-consistent policy: retained
ancestor count, minimum age, GC batch size, claim TTL, and retry backoff are all
positive bounded values. GC atomically claims only non-head, unreferenced
checkpoints outside the ancestor and age windows. An expired claim may be
reclaimed; only the exact live claim may finalize. Deletion leaves an immutable
tombstone: checkpoint identity, parent chain, operation and source fences,
manifest digest, and logical-byte audit remain while provider/object references
are cleared only after verified absence.

Portable cleanup enumerates the exact opaque checkpoint prefix independently of
the final manifest, so abandoned chunks and partial uploads cannot hide. Count
and byte bounds fail closed. After deletion, two empty inventories separated by
the configured consistency window and carrying the same digest are required;
any observed object resets the proof. MOA currently requires a dedicated
provider-verified unversioned checkpoint bucket. Unknown, enabled, suspended,
or changed versioning state blocks readiness and purge rather than leaving
recoverable historical object versions behind. Runtime observes the provider
before constructing mutation owners, refreshes through the same credential
provider halfway through the freshness window, and projects the shared gate
into readiness. A failed or unexpectedly exited refresh task invalidates the
gate and is process-fatal.

Periodic reconciliation compares durable rows only with provider resources
whose MOA ownership metadata, provider account, tenant, workspace, operation,
and generations verify exactly. Unrecognized resources are quarantined and
reported, never auto-deleted. Absence requires two separated empty observations
while renewing the same fenced cleanup claim.

Local/Docker inventory captures the same owner and generation tuple from the
fenced `HandSpec`, persists it in the lease payload, and restores it when a new
process adopts the lease. Inventory is filtered by the requested provider
account generation; an ownerless local resource cannot be used as absence
evidence. Daytona volumes use the exact account generation plus durable volume
reference because Daytona does not expose workspace metadata on volume-list
responses. E2B compute requires the complete authenticated metadata tuple,
including the hand provisioning operation.

The runtime owns these passes through one supervised workspace-reaper task.
Readiness remains false until the first complete operation-reconciliation,
checkpoint-retention, provider-inventory, and fleet-metric pass succeeds, and
becomes false again when its heartbeat is stale. An unexpected task exit is
process-fatal; graceful shutdown cancels and joins it within the process drain
deadline so no replica silently serves without a cleanup owner.

Tenant purge ordering is load-bearing:

1. fence all new workspace access and provider operations;
2. enumerate durable grants, operations, reservations, compute, storage
   resources, checkpoints, object versions, and wrapped keys;
3. delete or reconcile compute, provider volumes/subpaths, portable
   object bytes, and encryption material;
4. prove external absence under the same fences; and only then
5. delete ownership rows and enqueue inverse OpenFGA tuples.

A provider outage leaves purge incomplete and the workspace inaccessible. It
must not erase the reconciliation ledger or claim success.

Restate journals external deletion plus its secret-free, tenant/operation-bound
absence proof as one phase. Durable database confirmation is a separate phase
that consumes that exact journaled proof and performs no provider I/O. A crash
between them therefore retries confirmation only; it cannot resend provider or
object deletion.

That external phase currently uses the public high-cost workflow budget (360s
inactivity, 60s abort cleanup). Configuration rejects three mandatory absence
windows that alone consume that budget. Provider and object-store I/O must also
fit inside it; moving purge to smaller journaled pages is deferred with general
long-horizon workspace orchestration rather than hidden behind larger fixture
timeouts.

The external-absence transition is a database privilege boundary. Only the
`moa_workspace_maintenance` role may execute the narrow SECURITY DEFINER fence,
proof-confirmation, and proof-requirement functions. Those functions validate
the exact active tenant-wide destruction fence and operation before owner-local
transaction state may bypass write guards. `moa_app`, a caller-set purge GUC,
the wrong tenant, or the wrong operation cannot activate the bypass. The
durable absence digest is committed before relational purge may advance.

## Security And Public Surfaces

Workspace authorization is checked before every protected read and every
provider call. The local delete/lifecycle fence is checked in addition to
OpenFGA so cached authorization cannot reopen deleting state. A contact-private
worker workspace does not become visible to another contact in the same tenant;
delegated agents require both delegation and workspace use.

Provider credentials, sandbox access tokens, Daytona subpaths, E2B tokens,
object-store credentials, object keys, local paths, raw provider identifiers,
and file content never enter prompts, public DTOs, events, Restate state,
metrics labels, or logs. Trusted/runtime files are reinstalled from current
state outside the checkpoint root, so restore cannot resurrect old credentials,
authorization, policy, egress, or trusted-file status.

Every cloud hand handle and provider operation carries the workspace's
persisted `provider_account_id` and account generation. Provider discovery and
reaping require that context explicitly; provider names, request bodies, model
output, and process-local maps cannot select an account. Deployment-owned keys
are resolved from typed, operator-authored secret-file selectors at each
attempt. The file owner/mode and mapping generation are revalidated before a
fresh no-redirect, DNS-pinned `moa-security` client is built for the exact
allowlisted HTTPS origin.

A shared provider project or organization is a shared control-plane blast
radius even when MOA's data plane remains tenant-isolated. A credential may be
shared across tenants only when the provider contract guarantees equivalent
physical isolation inside that account; otherwise each tenant requires a
dedicated provider project/organization credential and isolation cell. The
non-secret project fingerprint is persisted with the provider account; the
secret never enters the tenant connection credential vault.

## Deployment And Rollout

`sandbox_workspaces.mode` is the single rollout switch and defaults to
`disabled`. Disabled replicas do not construct provider/object mutation owners,
start workspace maintenance, or admit workspace traffic. `maintenance` starts
retention, reconciliation, purge, deletion, and completion of already durable
operations while refusing every new workspace, writer claim, provisioning
request, and tool dispatch. `admit` includes maintenance and enables only the
deployment-owned canary account/generation/isolation cell for tenants in its
explicit allowlist with positive tenant/account quota routes.

Before maintenance or admission becomes ready, the runtime must prove V58,
exact OpenFGA model v7 with bootstrap enabled, durable KMS, an authenticated
checkpoint bucket/prefix observation matching the unversioned policy, complete
provider-account credentials and immutable project fingerprints, bounded quota
and retention policy, a dedicated `MOA_DATABASE_MAINTENANCE_URL` login that is
a member of the NOLOGIN `moa_workspace_maintenance` role, and a fresh supervised
reaper heartbeat. The maintenance credential is distinct from runtime/admin and
is injected only into the process-owned coordinator. `MOA_SKIP_FGA`,
missing or wrong OpenFGA, ephemeral KMS, unknown bucket versioning, missing
provider limits, and stale reaper state all fail before provider mutation or
listener readiness.

Production uses a dedicated Kubernetes service account with workload identity
and a reserved prefix in external object storage. The production overlay
contains no RustFS or static object-store key. Local RustFS is digest-pinned,
PVC-backed, probed, and used only by the deterministic local lane; local mode
remains disabled while its development stack deliberately skips OpenFGA.

Roll forward in this order: deploy V58 and OpenFGA v7 while disabled; drain and
reap legacy hands; enable the deterministic maintenance lane; configure one
provider account/cell and tenant canary; verify bucket, reconciliation, capacity,
and alerts; then switch to `admit` and expand the allowlist deliberately. The
supported rollback is `admit` to `maintenance`: stop admission/writer claims
atomically, keep maintenance and the reaper healthy, and drain/reconcile durable
operations. Never switch directly to disabled while durable work remains.

Multi-replica local persistence is unsupported unless compute control,
inventory, status, and destroy are globally reachable across replicas. Sticky
routing is not a correctness mechanism.

## Related Documents

- [Architecture Overview](01-architecture-overview.md)
- [Brain Orchestration](02-brain-orchestration.md)
- [Session And Event Log](05-session-event-log.md)
- [Hands And MCP](06-hands-and-mcp.md)
- [Security](08-security.md)
- [Restate Architecture](12-restate-architecture.md)
- [Architecture Policy](15-architecture-policy.md)

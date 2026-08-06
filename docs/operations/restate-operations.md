# Restate Operations

This runbook covers the shared snapshot repository, node replacement, and the
shutdown and placement invariants for MOA's Restate 1.7.2 cluster. Restate owns
durable orchestration state; Postgres remains MOA's product record as described
in [the Restate architecture](../12-restate-architecture.md).

## Safety boundaries

- Use `restatectl` against the node-to-node port (5122), not the Admin API.
- Drain a healthy node before stopping it. Never delete a Restate PVC first.
- Change one member at a time and keep the configured replication quorum live.
- Capture the effective server configuration before an incident or upgrade.
- Snapshots are partition bootstrap artifacts. They are not automated
  cluster-wide point-in-time recovery (PITR).
- Do not restore independently timed volume copies into a multi-node cluster.
  Restate does not currently automate the coordination or repair required to
  make those copies a consistent cluster backup.

The upstream references are [Snapshots and Backups][snapshots] and
[Highly-Available Clusters][clusters].

[snapshots]: https://docs.restate.dev/server/snapshots.md
[clusters]: https://docs.restate.dev/server/clusters.md

## Production snapshot provisioning

Production uses a dedicated, versioned GCS bucket with uniform bucket-level
access and public-access prevention. Platform infrastructure must provision a
dedicated Google service account with bucket-scoped object access and grant
Workload Identity impersonation to
`serviceAccount:PROJECT_ID.svc.id.goog[moa-restate/restate]`. Static
service-account keys are forbidden. Restate Operator 2.8.1 owns the fixed
`restate` Kubernetes service account, so it is not a configurable application
input.

Provide the pre-provisioned resources to the renderer:

```bash
export RESTATE_SNAPSHOT_BUCKET=GLOBALLY_UNIQUE_BUCKET
export RESTATE_SNAPSHOT_PREFIX=SNAPSHOT_PREFIX
export RESTATE_SNAPSHOT_GSA=SERVICE_ACCOUNT_NAME@PROJECT_ID.iam.gserviceaccount.com
export MOA_ORCHESTRATOR_IMAGE=registry.example/moa/orchestrator@sha256:DIGEST
export MOA_EDGE_IMAGE=registry.example/moa/edge@sha256:DIGEST
k8s/scripts/render-production.sh /new/output/directory
```

The bucket, prefix, and identity exist only in the temporary render workspace
and final deployment artifact. The checked-in overlay contains no fake
production bucket or cloud identity. Treat the rendered artifact as
environment-specific configuration.

## Snapshot policy

Production configures:

- `worker.durability-mode = "balanced"`;
- a snapshot after both 60 minutes and 100,000 applied records;
- `worker.snapshots.num-retained = 2`, retaining the two most recently
  published snapshots with the Restate 1.7 key; and
- a `gs://BUCKET/PREFIX` destination supplied at render time.

Both triggers must be satisfied when both are configured. Tune them from
observed partition catch-up time and the agreed availability objective rather
than treating 60 minutes or 100,000 records as an RPO guarantee.

Local Kubernetes uses the separate `moa-restate-snapshots` RustFS bucket at
`rustfs.moa-system.svc.cluster.local:9000`. Restate reads the local access and
secret keys from the `moa-restate/moa-restate-snapshots` Secret via supported
configuration environment overrides. Network policy admits only TCP/9000 from
the Restate pods and the existing local RustFS clients; port 9001 is not part of
the application path.

## Snapshot verification

Before node maintenance:

1. Run `restatectl status`, `restatectl config get`, and
   `restatectl nodes list --extra`.
2. Create and immediately trim to a fresh snapshot:

   ```bash
   restatectl snapshots create-snapshot --trim-log --yes
   ```

3. Run `restatectl partitions list`. Every partition leader must be `Active`
   and have a numeric `ARCHIVED` LSN.
4. Inspect `PREFIX/PARTITION_ID/latest.json` and its referenced objects in the
   object store for every partition.
5. Record the maximum `APPLIED - ARCHIVED` gap. This is a measured record gap,
   not a wall-clock RPO.

An inaccessible repository, a missing partition pointer, or an absent archived
LSN blocks maintenance.

## Safe member replacement

For a healthy member that can still participate in the cluster:

1. Confirm that the remaining members can maintain the configured log and
   partition replication.
2. Resolve the exact Restate node ID from `restatectl nodes list --extra`.
3. Exclude it from new log nodesets and partition replica sets:

   ```bash
   restatectl nodes set-storage-state \
     --nodes NODE_ID --storage-state read-only --yes
   restatectl nodes set-worker-state \
     --nodes NODE_ID --worker-state draining --yes
   ```

4. If it is a metadata-server member, remove it from the metadata Raft group:

   ```bash
   restatectl metadata-server remove-node NODE_ID --yes
   ```

5. Create snapshots with `--trim-log`. Wait until `partitions list` and
   `logs describe --all --extra` no longer reference the member.
6. Stop that one Restate pod. Only after it is stopped, remove its PVC.
7. Remove the stopped node's cluster entry:

   ```bash
   restatectl nodes remove --nodes NODE_ID --yes
   ```

8. Start the replacement with an empty PVC and the same effective cluster and
   snapshot configuration.
9. Require a new Restate node ID, active partition replicas on the replacement,
   numeric archived LSNs for every partition, and a healthy cluster.
10. Exercise a durable invocation that was in-flight across the replacement and
    verify it completes exactly once.

Do not skip directly to pod or PVC deletion. A pod restart with its original
PVC verifies process restart only; it does not verify snapshot bootstrap. A
copy of a volume verifies neither shared snapshots nor safe cluster recovery.

## Recovery drills

Run the snapshot-verification and safe-member-replacement procedures above in a
disposable environment before a production topology or storage change. Record
the archived LSN gap, replacement duration, new PVC and node identities, and an
exactly-once in-flight invocation result in the change record. The repository
does not automate destructive cluster or PVC mutation.

## Shutdown and placement

Production Restate uses a ten-minute server shutdown timeout and a 660-second
Kubernetes termination grace. Keep the Kubernetes grace strictly greater than
the Restate timeout so kubelet cannot force-kill a member during its drain.

The three Restate pods use required hostname anti-affinity plus mandatory
hostname and zone topology-spread constraints with three minimum domains. A
production cluster without three eligible hosts and zones must remain
unschedulable rather than silently co-locating durable replicas.

The ordinary local overlay is intentionally different: it renders one Restate
replica, labels that deployment `local-single-node`, and removes the production
anti-affinity and three-domain constraints. This keeps a one-node Kind cluster
schedulable without pretending it has zone durability. Run the live deployment
smoke only with an explicit local Kind context, for example:

```bash
k8s/scripts/smoke.sh --kind-context kind-moa-validation
```

The smoke verifies that Kind reports the named cluster, that the kubeconfig
cluster and user match that exact context, and that the API endpoint is
loopback. It refuses non-Kind, GKE-like, development, and production context
names before creating its temporary network-policy probe pod.

Kubernetes placement does not set Restate's `location`. Do not enable
location-aware replication until the Operator can inject a stable zone-derived
location into each Restate node's effective config. Until then, Kubernetes
enforces failure-domain placement and Restate continues to use node-count
replication.

## Effective configuration capture

Before an upgrade and after any configuration change, send `SIGUSR1` to each
Restate process and archive the resulting effective configuration from its logs.
Also record the `RestateCluster` generation, Restate image digest, Operator
version, node list, cluster config, and snapshot destination prefix. Do not copy
credential values into the record.

The production Restate patch fixes `log-format = "json"`,
`log-disable-ansi-codes = true`, and the bounded
`warn,restate=info` log filter. A process that starts with any other effective
values blocks rollout. The JSON output is an ingestion contract: do not add an
ANSI-stripping or free-form regex parser downstream to compensate for a server
misconfiguration.

## Local metrics, logs, traces, and control-plane evidence

The local LGTM overlay extends its pinned OpenTelemetry Collector with two
local-only pipelines. It discovers the one local Restate pod on port 5122 and
scrapes Prometheus metrics, and it reads only that co-located pod's CRI log path
from a read-only host mount. Restate owns the JSON body; the collector removes
only the CRI envelope and adds the bounded `service.name=restate` resource
identity. Production continues to use Alloy and does not use this host mount.

After creating representative traffic, run:

```bash
k8s/overlays/local/phase0-observability-report.sh
```

The report must show `up{job="restate"}`, Restate metric families, and JSON
Restate log records in addition to MOA's three OTLP signals. Use the product
cancellation path below for a disposable blocked turn; use Restate SQL, Tempo,
and Loki directly when incident correlation is needed.

## Product cancellation and Restate invocation controls

Use the narrowest control that matches the intended outcome:

| Control | Use | Consequence |
|---|---|---|
| MOA `Session/request_cancel` | A user or operator wants the active product turn stopped | Preserves the MOA cancellation path, cascades to owned children, and records terminal product cleanup. This is the default. |
| Restate cancel | The product path is unavailable but its deployment is reachable | Cooperatively throws cancellation at the next durable await and propagates through attached calls. Already-committed external effects are not undone unless the handler registered compensation. |
| Restate pause | Freeze a retrying invocation while investigating or preparing a compatible deployment | Leaves the invocation and journal retained; it consumes operational attention until explicitly resumed or cancelled. |
| Restate resume | Retry a paused/backing-off invocation on its pinned deployment | Re-executes only from the durable journal boundary. Resuming on another deployment is safe only when journal replay is compatible. |
| Restate kill | Cancellation cannot reach a permanently unavailable deployment | Stops the invocation tree without handler cleanup or compensation. Detached sends survive. Treat the product state as potentially inconsistent. |

For a single known invocation, use the Admin API rather than a service-wide
selector. Capture `sys_invocation` and `sys_journal` first. These commands mutate
durable control-plane state:

```bash
# Precondition: INVOCATION_ID was copied from current introspection evidence.
# Consequence: handler cancellation/cleanup runs asynchronously.
curl -X PATCH "${RESTATE_ADMIN_URL}/invocations/${INVOCATION_ID}/cancel"

# Precondition: the invocation is paused/backing off and its pinned deployment
# is healthy and journal-compatible. Consequence: execution retries now.
curl -X PATCH "${RESTATE_ADMIN_URL}/invocations/${INVOCATION_ID}/resume"

# Precondition: cancellation was attempted and cannot reach the deployment;
# incident owner accepts missing compensation and manual product repair.
# Consequence: immediate termination; detached sends and external effects remain.
curl -X PATCH "${RESTATE_ADMIN_URL}/invocations/${INVOCATION_ID}/kill"
```

Pause is also a mutation and must carry an incident/change reference:

```bash
# Consequence: no progress until an explicit resume/cancel/kill decision.
curl -X PATCH "${RESTATE_ADMIN_URL}/invocations/${INVOCATION_ID}/pause"
```

Never bulk-kill a service to repair one turn. After cancel or kill, reconcile
Postgres session/event state, connector effect ledgers, action reviews, worker
children, and detached sends before declaring the incident closed. Kill is the
last resort, not a faster cancel.

## One-minor Restate upgrade and rollback fence

MOA upgrades Restate by one minor at a time. Before changing the image:

1. Capture effective config and cluster topology; require a healthy quorum.
2. Publish and verify all partition snapshots and record the archived LSN gap.
3. Read every intervening Restate and Operator release note. Confirm that the
   target Operator supports both the current and target server minor.
4. Render the new Restate and MOA images by immutable digest. Keep the old
   artifact and its exact configuration available.
5. Run the bounded `restate-recovery-pr` nextest profile, the production digest
   renderer in the image-build workflow, and `k8s/scripts/smoke.sh` against a
   disposable local Kind cluster. Any journal mismatch (`RT0016`) blocks the
   upgrade.
6. Gate new MOA turn admission and wait for old pinned MOA deployments and
   active maintenance work to drain before removing anything.

Roll one Restate member at a time and require it to rejoin, catch up, and expose
healthy partition replicas before touching the next member. Stop immediately on
snapshot/archive lag, missing leaders, repeated invocation failures, or journal
mismatch.

Rollback is permitted only while the previous server minor explicitly supports
the metadata and storage format now on disk and no irreversible metadata
migration has run. Crossing that fence turns rollback into forward recovery:
keep quorum, stop the rollout, preserve volumes and snapshots, and move to the
fixed newer version. Never restore individual PVC copies over a partially
upgraded cluster.

## Immutable MOA deployment drain and journal mismatch

Restate pins an active invocation to the deployment that started its journal.
Register the new immutable endpoint and let new invocations route to it while
the old deployment drains. Require active invocations to remain pinned, new
invocations to select the new revision, and logs to contain no `RT0016`. Do not
overwrite or force-remove the old endpoint.

If a journal mismatch occurs, pause the exact invocation and retain its pinned
deployment. Compare the two image digests and handler flow at the reported
journal entry. Resume on a different deployment only when the new code emits
the identical journal prefix. Otherwise restore the original pinned endpoint
or cancel through the product path. Forcing deployment removal or killing the
invocation discards the safest recovery route.

## Hard product status cutover: `paused` to `idle`

Product `idle` means a session is healthy between turns. Restate `paused` means
an invocation cannot advance. The names must never be used interchangeably,
and the hard cut has no alias or dual reader.

Execute the cutover as a maintenance transaction:

1. Gate edge admission. Consequence: new messages are rejected or held outside
   MOA until the gate is restored; announce the window before enabling it. Apply
   separate GitOps sync waves in this order: edge admission gate, drain
   observation, migration-only Job and bootstrap, normal RestateDeployment
   readiness, then edge restoration.
2. Query `sys_invocation` and wait for active `Session`, `TurnExecution`, worker,
   `ExecutionRun`, `ExecutionTask`, and `ExecutionCompensation` invocations to
   finish. Cancel only through their product owners. The migration Job runs the
   complete current migration chain, so every hard-cut preflight in that chain
   must be satisfied before it starts.
3. Deploy the exact immutable migration-capable MOA image into the dedicated
   `moa-session-status-migrator-<image-revision>` Job. Its init container alone
   runs `migrate`; normal runtime pods have only a read-only cutover wait init
   container and no database migration credential. Do not execute
   `V000054__session_status_idle.sql` by hand. The owned migration runner applies
   V54 to `sessions.status` and live `SessionStatusChanged` payloads, then
   verifies, rewrites, and BLAKE3 re-digests immutable
   `session_event_archives.payload` BYTEA records in one locked transaction.
   Direct SQL would leave archived payloads and their integrity digests stale.
4. The bootstrap identity registers the Job's migration-only endpoint, which
   exposes only `StatusMigrationDispatcher` and the raw `Session` migration
   handler—not `Health`, `start_turn`, or any product handler. It then enumerates
   every session ID from Postgres and invokes the ingress-public
   `StatusMigrationDispatcher/migrate` maintenance service once per key. The
   dispatcher immediately calls the handler-private typed
   `Session/migrate_status_idle(Json<SessionStatusIdleMigrationRequest>)`
   operation service-to-service. It raw-reads and rewrites `K_STATUS` and
   `K_META`; do not route this operation through the new `SessionStatus`
   deserializer or expose it through edge routes.
5. Send the exact request body `{"session_id": "SESSION_UUID"}`. Require every
   response to report `retired_values_remaining = 0` and neither `status` nor
   `meta_status` equal to `paused`. Postgres enumeration is not a complete
   Restate-state inventory: an orphaned Session virtual object might no longer
   have a `sessions` row. Before restoring edge, query Restate's global `state`
   table for every `Session` `status` and `meta.status` value. Record the query
   and count, but no state payloads, in the change evidence and keep edge gated
   if the count is nonzero. Also run the Postgres checks:

   ```sql
   SELECT count(*) FROM sessions WHERE status = 'paused';
   SELECT count(*) FROM events
   WHERE event_type = 'SessionStatusChanged'
     AND (payload #>> '{data,from}' = 'paused'
          OR payload #>> '{data,to}' = 'paused');
   SELECT count(*) FROM state
   WHERE service_name = 'Session'
     AND ((key = 'status' AND value_utf8 = '"paused"')
       OR (key = 'meta' AND value_utf8 LIKE '%"status":"paused"%'));
   ```

   The first two queries run in Postgres; the third runs through Restate SQL.
   Every count must be zero.
6. After its per-key migration responses and Postgres verification succeed,
   bootstrap deregisters the migration-only endpoint and records
   `session_status_idle_v54` in `deployment_cutover_receipts`. Every normal
   runtime pod blocks in `wait-status-cutover` until that admin-verified receipt
   exists. Perform the independent global Restate state query above and wait
   until the `RestateDeployment` has observed the applied generation and all
   desired replicas are Ready. Edge remains at zero through those checks and is
   applied as a separate final wave. If the global
   count is nonzero, keep admission gated and use the dedicated migration image
   to register the migration-only endpoint again, migrate or explicitly retire
   each orphaned key, and rerun the proof. Do not deploy an alias for `paused`.

On later image revisions, the migration Job init container still owns the
complete forward SQL chain, but its handler container exits as soon as it sees
the existing V54 receipt. Bootstrap verifies the receipt and Postgres state,
skips registration of that completed migration endpoint, and proceeds to the
new steady-state runtime registration. Revision-derived Job names keep both
stages immutable across A-to-B rollouts.

Scaling edge to zero, cancelling turns, and running the migrations interrupt
traffic and rewrite durable state. Record the preflight counts, operator,
artifact digests, migration results, and admission-gate restoration in the
change log. A failed cutover is recovered forward from those records; it is not
rolled back by reintroducing the retired spelling.

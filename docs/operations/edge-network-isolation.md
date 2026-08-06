# Edge network isolation

`moa-edge` is the only MOA service that should be exposed to untrusted
networks. It validates credentials, injects trusted `X-Moa-*` identity
headers, and forwards public calls to the internal Restate ingress. Restate
then invokes `moa-orchestrator` on port 9080, where handlers trust those
headers absolutely. Anyone who can reach either Restate ingress or port 9080
directly can impersonate any user or agent.

Connector credential writes are the one deliberate non-Restate route. The
edge forwards opaque credential bytes to the orchestrator's private listener
on port 10023 so secret material never enters the Restate journal. That
listener must remain internal and reachable only from `moa-edge`; it is not a
second public API surface.

`MOA_EDGE_CONNECTOR_MANAGEMENT_ENABLED` is the independent public-surface
rollout switch. It defaults to false. While false, connection management and
the credential PUT both return 404 before
authentication, JSON translation, Restate forwarding, or private credential
proxying. This switch does not make port 10023 safe to expose.

## Compose

Orchestrator handler port 9080 is bound to the compose internal network only.
The connector credential listener at `moa-orchestrator:10023` is likewise
internal-only and has no host port binding. `moa-edge` reaches it through
`MOA_EDGE_CONNECTOR_CREDENTIAL_UPSTREAM`.
Local Compose explicitly sets `MOA_EDGE_CONNECTOR_MANAGEMENT_ENABLED=true` for
development. Set it false in a local override when testing Checkpoint A.
The default `docker-compose.yml` is a development stack, not an isolation
boundary: it publishes Restate ingress, admin, and node ports on host ports
`10010`/`10011`/`10012`, so anyone who can reach those host ports can call the
trusted Restate ingress directly. Do not expose the compose stack to untrusted
networks. If a developer needs localhost access for direct handler debugging,
bind `127.0.0.1:10020:9080` in a developer-only override.

## Production / Kubernetes

- The orchestrator Service is `ClusterIP` or internal, never `LoadBalancer` or
  `NodePort`.
- Clusters must enforce Kubernetes `NetworkPolicy`, or an equivalent service
  mesh authorization policy. The network boundary is the only production
  defense for the trusted Restate handler surface.
- The Restate ingress boundary is covered by
  `RestateCluster.spec.security.networkPeers`: Restate ingress accepts
  `moa-edge`, and Restate admin accepts `moa-orchestrator`.
- The base Kubernetes manifests add a `NetworkPolicy` for
  `moa-orchestrator` pods. Port 9080 accepts traffic only from Restate pods in
  `moa-restate` labeled `moa.dev/restate-cluster: moa-restate`; edge
  pods must not call 9080 directly.
- The orchestrator `ClusterIP` Service exposes credential port 10023 only
  inside the cluster. The same `NetworkPolicy` permits that port only from
  `moa-edge` pods in `moa-system`; Restate and other workloads cannot call it.
- The base edge Deployment explicitly sets
  `MOA_EDGE_CONNECTOR_MANAGEMENT_ENABLED=false`. Enabling connectors is a later
  reviewed edge rollout, not a schema-migration side effect.
- Health port 9081 remains reachable for Kubernetes probes and local overlay
  readiness checks. Metrics port 9090 is allowed from the Alloy pods in the
  `observability` namespace. The SCIM listener default port 10022 is not
  exposed by the Kubernetes Service and has no NetworkPolicy allow rule; add a
  Service and a targeted allow rule before exposing SCIM in Kubernetes.
- If a service mesh is in use, require mTLS and enforce the same source/port
  policy between `moa-edge`, Restate, and `moa-orchestrator`.
- Verify exposure on deploy:
  `kubectl get svc moa-orchestrator -o jsonpath='{.spec.type}'` returns
  `ClusterIP`.
- Verify during deploy that the base render includes the orchestrator
  NetworkPolicy before applying:

  ```bash
  kubectl kustomize k8s/base >/tmp/moa-k8s-render.yaml
  rg -n "kind: NetworkPolicy|moa-orchestrator|part-of: moa" /tmp/moa-k8s-render.yaml
  ```

- Verify the public surface remains dark before Checkpoint A:

  ```bash
  curl -i https://<edge>/v1/connectors/connections
  ```

  Expected: the route returns 404 even without credentials. For planned rollback,
  suspend affected connection generations before restoring the switch to
  false; never expose the private listener as a rollback shortcut.

## Failure mode

A misconfigured deployment that exposes 9080 publicly bypasses authentication
and authorization entirely. Exposing credential port 10023 also bypasses the
edge's public request boundary and risks sending secrets through an unintended
path. Treat either exposure as a release-blocking deployment failure.

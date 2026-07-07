# Edge network isolation

`moa-edge` is the only MOA service that should be exposed to untrusted
networks. It validates credentials, injects trusted `X-Moa-*` identity
headers, and forwards public calls to the internal Restate ingress. Restate
then invokes `moa-orchestrator` on port 9080, where handlers trust those
headers absolutely. Anyone who can reach either Restate ingress or port 9080
directly can impersonate any user or agent.

## Compose

Restate ingress and orchestrator port 9080 are bound to the compose internal
network only, never to host `0.0.0.0`. The default `docker-compose.yml` exposes
only `moa-edge` publicly. If a developer needs localhost access for direct
handler debugging, bind `127.0.0.1:10020:9080` in a developer-only override.

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
  `moa-restate` labeled `moa.hwuiwon.com/restate-cluster: moa-restate`; edge
  pods must not call 9080 directly.
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

## Failure mode

A misconfigured deployment that exposes 9080 publicly bypasses authentication
and authorization entirely. There is no in-band defense; the design relies on
the network boundary. Treat this as a release-blocking deployment failure.

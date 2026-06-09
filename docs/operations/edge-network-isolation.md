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
- A NetworkPolicy permits ingress to Restate ingress only from the `moa-edge`
  pod selector and ingress to orchestrator port 9080 only from Restate.
- If a service mesh is in use, require mTLS between `moa-edge` and the
  orchestrator.
- Verify exposure on deploy:
  `kubectl get svc moa-orchestrator -o jsonpath='{.spec.type}'` returns
  `ClusterIP`.

## Failure mode

A misconfigured deployment that exposes 9080 publicly bypasses authentication
and authorization entirely. There is no in-band defense; the design relies on
the network boundary. Treat this as a release-blocking deployment failure.

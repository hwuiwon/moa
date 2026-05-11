# Edge network isolation

`moa-edge` is the only MOA service that should be exposed to untrusted
networks. The orchestrator (`moa-orchestrator`, port 9080) trusts the
`X-Moa-*` identity headers absolutely. Anyone who can reach port 9080 can
impersonate any user or agent.

## Compose

Port 9080 is bound to the compose internal network only, never to host
`0.0.0.0`. The default `docker-compose.yml` removes the public mapping. If a
developer needs localhost access for debugging, bind `127.0.0.1:10020:9080` in
a developer-only override.

## Production / Kubernetes

- The orchestrator Service is `ClusterIP` or internal, never `LoadBalancer` or
  `NodePort`.
- A NetworkPolicy permits ingress to port 9080 only from the `moa-edge` pod
  selector.
- If a service mesh is in use, require mTLS between `moa-edge` and the
  orchestrator.
- Verify exposure on deploy:
  `kubectl get svc moa-orchestrator -o jsonpath='{.spec.type}'` returns
  `ClusterIP`.

## Failure mode

A misconfigured deployment that exposes 9080 publicly bypasses authentication
and authorization entirely. There is no in-band defense; the design relies on
the network boundary. P1.10 adds a deploy-time runbook check.

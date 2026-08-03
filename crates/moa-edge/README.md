# moa-edge

Public HTTP edge for MOA (the `moa-edge` binary). The edge terminates incoming
credentials, resolves identity, strips any inbound identity headers, and
forwards requests to Restate ingress with trusted identity headers injected.

## Modules

- `routes` — HTTP routes exposed by the edge service.
- `proxy` — forwarding HTTP proxy with identity-header injection.
- `connector_credential_proxy` — bounded exact-path forwarding to the private
  orchestrator credential ingress; it never targets Restate or accepts an
  upstream response body.
- `ingress` — Restate ingress path construction.
- `headers` — the header contract between `moa-edge` and `moa-orchestrator`.
- `mcp` — tenant-operations Model Context Protocol transport and tools (an
  inbound operator surface).
- `tenant_accounts` — tenant-account application and persistence boundaries.

The edge binary requires `MOA_EDGE_CONNECTOR_CREDENTIAL_UPSTREAM` to be the
origin-only URL of the private orchestrator credential listener. That listener
must be reachable only from edge workloads.

`MOA_EDGE_CONNECTOR_MANAGEMENT_ENABLED` defaults to `false`. While false, the
complete connector management/source/credential subtree returns 404
before authentication, translation, Restate forwarding, or private proxying.
Local Compose explicitly opts in; the Kubernetes base remains dark until the
reviewed rollout checkpoint.

## Features

- `auth0` — enables the Auth0 backend in `moa-auth-providers`.

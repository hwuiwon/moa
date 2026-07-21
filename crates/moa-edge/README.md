# moa-edge

Public HTTP edge for MOA (the `moa-edge` binary). The edge terminates incoming
credentials, resolves identity, strips any inbound identity headers, and
forwards requests to Restate ingress with trusted identity headers injected.

## Modules

- `routes` — HTTP routes exposed by the edge service.
- `proxy` — forwarding HTTP proxy with identity-header injection.
- `ingress` — Restate ingress path construction.
- `headers` — the header contract between `moa-edge` and `moa-orchestrator`.
- `mcp` — tenant-operations Model Context Protocol transport and tools (an
  inbound operator surface).
- `tenant_accounts` — tenant-account application and persistence boundaries.

## Features

- `auth0` — enables the Auth0 backend in `moa-auth-providers`.

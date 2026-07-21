# Local port map

MOA local development reserves host ports in grouped blocks starting at
`10000`. Container-internal ports do not change; this file tracks only ports
bound on the developer machine.

| Group | Host port | Service | Purpose | Internal target |
|---|---:|---|---|---|
| Public edge | 10000 | `moa-edge` | Public HTTP edge for local API calls | `moa-edge:8080` |
| Public edge | 10001 | `moa-edge` | Prometheus metrics for the edge layer | `moa-edge:9090` |
| Restate | 10010 | `restate` | Restate ingress for handler invocation | `restate:8080` |
| Restate | 10011 | `restate` | Restate admin API and web UI | `restate:9070` |
| Restate | 10012 | `restate` | Restate node endpoint | `restate:5122` |
| Orchestrator | 10020 | `moa-orchestrator` | Optional direct handler debug port; not exposed by default compose | `moa-orchestrator:9080` |
| Orchestrator | 10021 | `moa-orchestrator` | Health and readiness probes | `moa-orchestrator:9081` |
| Orchestrator | 10022 | `moa-orchestrator` | SCIM v2 provisioning listener | `moa-orchestrator:10022` |
| Orchestrator | 10023 | `moa-orchestrator` | Prometheus metrics; scraped by loadtest reports | `moa-orchestrator:9090` |
| Authorization | 10030 | `openfga` | OpenFGA HTTP API | `openfga:8080` |
| Authorization | 10031 | `openfga` | OpenFGA gRPC API | `openfga:8081` |
| Authorization | 10032 | `openfga` | OpenFGA Playground | `openfga:3000` |
| Data | 10040 | `postgres` | Postgres for MOA and OpenFGA logical databases | `postgres:5432` |
| Privacy | 10050 | `moa-pii-service` | PII classifier sidecar API | `moa-pii-service:8080` |
| Runtime cache | 10051 | `valkey` | Runtime cache store (Redis protocol) | `valkey:6379` |
| Analytics | 10061 | `clickhouse` | ClickHouse HTTP interface for lineage analytics | `clickhouse:8123` |
| Object storage | 9000 | `rustfs` | S3-compatible API for session attachments | `rustfs:9000` |
| Object storage | 9001 | `rustfs` | RustFS web console | `rustfs:9001` |

RustFS host ports are overridable via `MOA_RUSTFS_PORT` (default `9000`) and
`MOA_RUSTFS_CONSOLE_PORT` (default `9001`).

## Rules

- Add new host ports in the nearest group block, leaving room for related
  services.
- Do not expose `moa-orchestrator:9080` from compose by default. If direct
  host debugging is needed, use a local override that binds
  `127.0.0.1:10020:9080`.
- Keep service-to-service compose URLs on internal container ports. For
  example, `moa-edge` talks to `http://restate:8080`, not
  `http://localhost:10010`.

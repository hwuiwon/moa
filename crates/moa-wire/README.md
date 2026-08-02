# moa-wire

Shared wire DTOs for the cloud orchestrator HTTP surface: the
request/response types exchanged over MOA's public HTTP edge and internal
service boundaries. Types depend on base domain types from `moa-core` but
carry no runtime logic, so consumers that only speak the wire format (edge,
orchestrator, analytics, ...) do not rebuild the core runtime crates.

## Modules

One module per service surface:

- `admin` — administrative maintenance
- `agents` — configured-agent service
- `analytics` — analytics service
- `artifacts` — artifact service
- `experiments` — experiment and agent-revision simulation
- `knowledge` — tenant knowledge-base service
- `lineage` — lineage administration
- `memory` — graph-memory service
- `privacy` — privacy export and erasure
- `session_store` — session-store service
- `skills` — skill import, export, and review
- `tenants` — tenant lifecycle operations
- `tools` — tool descriptors
- `turn` — turn workflow and session progress

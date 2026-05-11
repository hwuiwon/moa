# Changelog

## Unreleased

### Breaking

- Removed `moa daemon serve`. The orchestrator now runs as a standalone
  Restate-backed service. For local development, run `make dev`; for shared
  environments, set `MOA__ORCHESTRATOR__ENDPOINT` or
  `[orchestrator].endpoint` in `~/.moa/config.toml`.
- Removed `moa daemon start`, `moa daemon stop`, and `moa daemon logs`. Use
  `docker compose logs moa-orchestrator` for local logs and the Restate UI at
  `http://localhost:10011` for invocation visibility.
- `moa daemon status` now health-probes the configured orchestrator endpoint
  instead of inspecting an in-process daemon socket.

### Deprecated

- `MoaConfig.daemon` is superseded by `MoaConfig.orchestrator`. The daemon
  fields remain readable for one release so older config files do not fail to
  deserialize.

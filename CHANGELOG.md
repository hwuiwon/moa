# Changelog

## Unreleased

### Breaking

- Removed `moa daemon serve`. The orchestrator now runs as a standalone
  Restate-backed service. For local development, run `make dev`; for shared
  environments, set `MOA_ORCHESTRATOR_ENDPOINT` or
  `[orchestrator].endpoint` in `~/.moa/config.toml`.
- Removed `moa daemon start`, `moa daemon stop`, and `moa daemon logs`. Use
  `docker compose logs moa-orchestrator` for local logs and the Restate UI at
  `http://localhost:10011` for invocation visibility.

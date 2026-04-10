# Step 23: Cloud Deployment (Fly.io + Turso Sync)

## What this step is about
Production cloud deployment: Fly.io for brain hosting, Turso Cloud for session sync, and the `moa sync enable` migration path.

## Files to read
- `docs/02-brain-orchestration.md` — Fly.io configuration
- `docs/05-session-event-log.md` — Turso sync
- `docs/10-technology-stack.md` — Deployment configs

## Tasks
1. **Dockerfile**: Multi-stage build, release binary with `cloud` features.
2. **`fly.toml`**: Configuration for Fly.io Machines with auto-suspend, auto-start, scale-to-zero.
3. **Turso sync**: When `cloud.turso_url` is set, `TursoSessionStore` connects to cloud with embedded replica. Local reads, cloud writes.
4. **`moa sync enable`** CLI command: Adds Turso sync URL to config, triggers initial sync.
5. **Memory sync**: When cloud mode is active, memory files sync via Turso file storage or a separate file sync mechanism.
6. **Health endpoint**: HTTP `/health` for Fly.io health checks.
7. **Graceful shutdown**: Handle SIGTERM from Fly.io, complete active turns, persist state.
8. **CI/CD**: GitHub Actions workflow for building and deploying to Fly.io.

## Deliverables
`Dockerfile`, `fly.toml`, `.github/workflows/deploy.yml`, updated `moa-session` for Turso Cloud, `moa sync` CLI command.

## Acceptance criteria
1. `fly deploy` succeeds and machine starts
2. Machine auto-suspends after 5 minutes idle
3. Machine auto-resumes on incoming message (< 1s)
4. Turso sync works: local changes appear in cloud, cloud changes appear locally
5. `moa sync enable` transitions a local install to cloud sync
6. Health endpoint returns 200
7. Graceful shutdown completes active turns

## Tests
- Integration test: Deploy to Fly.io staging, send message via Telegram, verify response
- Integration test: Turso sync — write event locally, verify it appears in cloud
- Load test: 10 concurrent sessions on one machine
- Chaos test: Kill machine mid-turn, verify session resumes on new machine

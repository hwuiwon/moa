# Event Replay Instrumentation Runbook

Use this runbook to validate whether per-turn session-event replay is a meaningful cost in long sessions.

## Goal

Measure how `SessionStore::get_events()` scales with turn count:

- `events_replayed`
- `events_bytes`
- `get_events_calls`
- `get_events_total_ms`
- `pipeline_compile_ms`

If replay work grows materially across turns, step 86-style snapshots become higher priority.

## How To Collect Data

1. Build MOA with observability enabled:

   ```bash
   cargo build --workspace
   ```

2. Start your local telemetry stack if you use one (Jaeger, Tempo, Grafana, or an OpenTelemetry collector).

3. Run a multi-turn local session against a workspace that requires repeated reads and tool use:

   ```bash
   cargo run -p moa-cli -- exec "Refactor the auth module and verify it still works"
   ```

4. Capture the per-turn replay summary log lines:

   ```text
   turn=37 get_events_calls=6 events_replayed=4213 events_bytes=918274 get_events_total_ms=128 pipeline_compile_ms=204
   ```

5. In tracing backends, inspect `session_turn` spans and chart:

   - `moa.turn.events_replayed`
   - `moa.turn.events_bytes`
   - `moa.turn.get_events_calls`
   - `moa.turn.get_events_total_ms`
   - `moa.turn.pipeline_compile_ms`

## How To Interpret It

- If `events_replayed` grows roughly linearly with turn number, the cumulative replay cost is trending toward O(N²).
- If `get_events_total_ms` stays below about 5% of total turn latency, replay is probably not the next bottleneck.
- If `pipeline_compile_ms` rises in lockstep with `events_replayed`, the history build is replay-bound.
- If `get_events_calls` is unexpectedly high on every turn, inspect call sites before building bigger caching or snapshot systems.

## Decision Rule

- Prioritize snapshotting if replay counts and replay latency both climb noticeably over a 30–40 turn session.
- Prefer deleting redundant `get_events()` call sites first if the count inflation comes from avoidable repeated reads inside one turn.

# Step 80 — Event Replay Count Instrumentation

_Count how many events are replayed per turn and expose it as an OTel span attribute and a log line. This single number will validate (or refute) the "O(N²) session cost" hypothesis and determines whether step 86 (context snapshot) is the right thing to do next._

---

## 1. What this step is about

In `moa-orchestrator/src/local.rs:run_session_task`, every turn calls `session_store.get_events(session_id.clone(), EventRange::all()).await?` — sometimes multiple times. The cost of this call grows linearly with session length. Across a 40-turn session, the total event-read work is O(N²).

We don't know:
- How many events a typical turn replays.
- How that count grows with turn number.
- Whether the cost shows up as measurable latency in the pipeline compile stage.
- Whether all the `get_events` call sites are actually necessary (some may be defensive duplication that can be removed outright — faster fix than step 86).

Before building a snapshot mechanism, prove the bottleneck exists and size it.

---

## 2. Files to read

- `moa-orchestrator/src/local.rs` — every call site of `session_store.get_events`. Grep confirms at least 5 per turn loop.
- `moa-session/src/postgres.rs` and `moa-session/src/turso.rs` — where `get_events` is implemented.
- `moa-brain/src/pipeline/history.rs` — the pipeline also loads events.
- Existing OTel span setup for the turn loop (steps 39–41). We extend those spans with new attributes.

---

## 3. Goal

1. Every call to `SessionStore::get_events` records three numbers: events returned, bytes returned (approximate), wall-clock duration.
2. The outer `session_turn` span shows an aggregate: total events replayed, total bytes deserialized, total time spent in `get_events` for the turn.
3. One structured log line per turn prints the growth curve:
   ```
   turn=37 events_replayed=4213 get_events_calls=6 get_events_total_ms=128 pipeline_compile_ms=204
   ```
4. Collected data after a 40-turn test session either confirms the O(N²) hypothesis (growth is linear per turn → quadratic cumulative) or refutes it.

---

## 4. Rules

- **Instrument at the trait boundary.** Wrap the `SessionStore` trait impl. Every call site gets counted without touching individual call sites. A per-call tracing span is cheap; don't add field-level instrumentation inside the Postgres/SQLite query code.
- **Count calls per turn, not globally.** Use a `tokio::task_local!` counter that resets at turn start, or a per-task `Arc<AtomicU64>` passed via a `TurnContext`. Global counters will be useless because they mix data from all concurrent sessions.
- **Do not block turn completion on metric export.** Metrics go through the existing OTel pipeline. If export is slow, it shouldn't delay the brain loop.
- **Bytes are approximate.** Serde-deserialized event records can be sized by `std::mem::size_of_val` on the Vec, or by estimating the total payload length during deserialization. Either is fine. Exact byte counts would require re-serializing, which defeats the purpose.
- **Don't fix the hypothesized problem in this step.** If the instrumentation reveals that two of the six `get_events` calls per turn are redundant, flag it in an issue and let step 86 handle the real fix. Keep this step tightly scoped to observation.

---

## 5. Tasks

### 5a. `CountedSessionStore` wrapper (preferred) or inline counter

Option A — a wrapping type:

```rust
// moa-orchestrator/src/instrumented_store.rs (new)
pub struct CountedSessionStore {
    inner: Arc<SessionDatabase>,
}

#[async_trait]
impl SessionStore for CountedSessionStore {
    async fn get_events(
        &self,
        session_id: SessionId,
        range: EventRange,
    ) -> Result<Vec<EventRecord>> {
        let start = Instant::now();
        let result = self.inner.get_events(session_id.clone(), range).await?;
        let duration = start.elapsed();
        let bytes = approx_bytes(&result);
        EVENT_REPLAY_COUNTER.record(result.len() as u64, duration, bytes);
        Ok(result)
    }
    // other methods: transparent pass-through
}
```

Wire it in `LocalOrchestrator::new` so every downstream consumer (router, brain, pipeline) sees the counted store.

Option B — a `TurnContext` with counters, passed explicitly. Rejected because it requires touching every call site and changes function signatures.

Pick Option A.

### 5b. `TURN_COUNTERS` task-local state

Counters must reset per turn. Use `tokio::task_local`:

```rust
tokio::task_local! {
    pub static TURN_COUNTERS: TurnCounters;
}

#[derive(Default, Debug)]
pub struct TurnCounters {
    pub get_events_calls: AtomicU64,
    pub events_returned: AtomicU64,
    pub bytes_returned: AtomicU64,
    pub total_duration_us: AtomicU64,
}
```

`CountedSessionStore::get_events` does `TURN_COUNTERS.try_with(|c| ...)` and increments. If not inside a turn (e.g., during session listing, startup probes), it's a no-op.

`run_session_task` wraps each turn with:

```rust
TURN_COUNTERS.scope(TurnCounters::default(), async move {
    // existing per-turn body
    // at end: snapshot counters, emit log + span attrs
}).await
```

### 5c. Emit at turn boundary

At the end of `record_turn_boundary` (or wherever the turn's outer span is closing):

```rust
TURN_COUNTERS.with(|c| {
    let calls = c.get_events_calls.load(Ordering::Relaxed);
    let events = c.events_returned.load(Ordering::Relaxed);
    let bytes = c.bytes_returned.load(Ordering::Relaxed);
    let us = c.total_duration_us.load(Ordering::Relaxed);

    tracing::Span::current()
        .record("moa.turn.get_events_calls", calls)
        .record("moa.turn.events_replayed", events)
        .record("moa.turn.events_bytes", bytes)
        .record("moa.turn.get_events_total_ms", us / 1000);

    tracing::info!(
        turn_number,
        get_events_calls = calls,
        events_replayed = events,
        events_bytes = bytes,
        get_events_total_ms = us / 1000,
        "turn event replay summary"
    );
});
```

Declare the new span attribute names in the `session_turn` span definition so tracing knows about them up front.

### 5d. Pipeline-internal `get_events` call (stage 6)

The HistoryCompiler also calls `get_events`. Ensure the counted wrapper is reached there too. If the compiler holds a direct reference to the raw `SessionDatabase`, switch it to accept `Arc<dyn SessionStore>` (which the counted wrapper implements). Verify via the integration test from step 78 that the counter increments during pipeline compile.

### 5e. Byte estimation helper

```rust
fn approx_bytes(events: &[EventRecord]) -> u64 {
    events.iter().map(|e| {
        std::mem::size_of::<EventRecord>() as u64
            + event_payload_size(&e.event) as u64
    }).sum()
}

fn event_payload_size(ev: &Event) -> usize {
    match ev {
        Event::UserMessage { text, .. } => text.len(),
        Event::BrainResponse { text, .. } => text.len(),
        Event::ToolCall { input, .. } => serde_json::to_string(input).map(|s| s.len()).unwrap_or(0),
        Event::ToolResult { output, .. } => output.to_text().len(),
        Event::ToolError { error, .. } => error.len(),
        Event::Checkpoint { summary, .. } => summary.len(),
        _ => 64, // small control events
    }
}
```

Exact byte accuracy is not the point; growth curves are.

### 5f. Data-collection runbook

Document at the bottom of this pack how to use it:

```
1. Build with: cargo build --features "observability"
2. Start a local OTel collector (docker run -p 4317:4317 -p 4318:4318 otel/opentelemetry-collector)
3. Run a 40-turn test session: moa exec "refactor the auth module" against a seeded workspace
4. Query spans by name "session_turn"
5. Plot moa.turn.events_replayed vs moa.turn.number
6. If linear in N, confirm step 86 (snapshot) is the right fix.
   If flat or sub-linear, investigate which calls are the top contributors and consider killing them outright.
```

---

## 6. Deliverables

- [ ] `moa-orchestrator/src/instrumented_store.rs` (or equivalent) — `CountedSessionStore` wrapper.
- [ ] `TURN_COUNTERS` task-local used in `run_session_task`.
- [ ] Turn-boundary log line and span attributes.
- [ ] Byte estimation helper in a utility module.
- [ ] Step 78 integration test gains an assertion that after a 7-turn session, `events_replayed` > 0 and has grown between early and late turns.
- [ ] Runbook in `moa/docs/` for how to collect the data and interpret it.

---

## 7. Acceptance criteria

1. Running `moa exec` on any session produces a per-turn log line containing `events_replayed`.
2. In a 10-turn local session, the `events_replayed` count at turn 10 is measurably higher than at turn 1 (confirms the O(N) per-turn hypothesis in the small).
3. OTel traces in Jaeger show `moa.turn.events_replayed` as a span attribute on `session_turn`.
4. No new hot spots in `cargo flamegraph` on a test session (the wrapper is supposed to be negligible overhead).
5. Data from a 40-turn session answers the question: is the cumulative `get_events_total_ms` across all turns a meaningful fraction (>5%) of total session wall clock? If yes, step 86 is confirmed as high priority. If no, step 86 moves behind caching work.

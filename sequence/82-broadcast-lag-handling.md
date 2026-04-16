# Step 82 — Broadcast Channel Lagged-Error Handling

_The per-session `event_tx` (broadcast 256) and `runtime_tx` (broadcast 512) channels silently drop messages for slow subscribers. Today `Lagged(n)` errors propagate unhelpfully. Make lag explicit, countable, and recoverable._

---

## 1. What this step is about

Every active session holds two broadcast channels:
- `event_tx: broadcast::Sender<EventRecord>` — buffer 256
- `runtime_tx: broadcast::Sender<RuntimeEvent>` — buffer 512

Subscribers include the TUI, the API layer, and future observers (Langfuse exporter, audit stream, etc.). If any subscriber falls behind — for example, the TUI pauses while the user scrolls history — the broadcast buffer fills. Once full, `recv()` returns `RecvError::Lagged(n)`. Most consumer code treats `Lagged` as a terminal error and bails out; in practice events silently disappear and the UI glitches.

At 500 QPS this goes from "occasionally a glitch" to "consistent data loss." We need three things:
1. Every lag event counted and logged with the session ID and the number of events dropped.
2. Consumers that expect "best-effort live preview" (like TUI follow-mode) continue after lag, with a one-line gap indicator.
3. Consumers that require ordered completeness (like the session search indexer) re-fetch from the durable event log rather than recovering from broadcast alone.

This step is observation + graceful degradation. It does not try to eliminate lag; that's a sizing decision for step 101.

---

## 2. Files to read

- `moa-orchestrator/src/local.rs` — where both channels are created. Subscribers get handles via `observe()` / `observe_runtime()`.
- `moa-core/src/event_stream.rs` (or wherever `EventStream` is defined) — the abstraction that bridges broadcast → consumer.
- Any TUI code that calls `orchestrator.observe(...)`. Grep for `.recv().await` on broadcast receivers.
- Any gateway/adapter code that subscribes to live streams.

---

## 3. Goal

1. Every `RecvError::Lagged(n)` returned by any broadcast receiver in MOA is handled explicitly — never silently swallowed, never treated as fatal where the consumer semantics are "best-effort live preview."
2. A single metric counter tracks lag events: total count, total events dropped, broken down by channel (event vs runtime) and session.
3. Consumers that declare themselves "best-effort" (TUI, UI preview) emit a gap indicator (`⚠ N events missed`) and continue.
4. Consumers that declare themselves "complete-ordered" re-fetch missing events from `SessionStore::get_events` starting from the last-seen sequence number, then rejoin the live stream.

---

## 4. Rules

- **`Lagged` is not an error; it's a signal.** Don't log it at `error!` level. Use `warn!` with full context.
- **Count every `Lagged`.** Even if the consumer recovers transparently, the metric counter must tick. This is the data we need to size channels in step 101.
- **Expose consumer policy explicitly.** Add a `LagPolicy` enum to the subscription API:

  ```rust
  pub enum LagPolicy {
      /// Skip missed events, emit a gap marker. Used by live TUI, preview UIs.
      SkipWithGap,
      /// Re-fetch missed events from the durable log. Used by indexers, audit.
      BackfillFromStore,
      /// Terminate the subscriber on lag. Used by automated consumers that can be restarted.
      Abort,
  }
  ```

- **Default policy is `SkipWithGap`.** Existing TUI consumers get sensible behavior without code changes.
- **Don't change channel sizes in this step.** Sizing is a separate concern (step 101). This step is about making sizing decisions measurable.

---

## 5. Tasks

### 5a. Define `LaggedReason` and extend `EventStream`

In `moa-core/src/event_stream.rs`:

```rust
pub enum LiveEvent {
    Event(EventRecord),
    Gap { count: u64, channel: BroadcastChannel, since_seq: Option<SequenceNum> },
}

pub enum BroadcastChannel { Event, Runtime }

pub struct EventStream {
    // existing fields...
    lag_policy: LagPolicy,
}

impl EventStream {
    pub fn with_lag_policy(mut self, policy: LagPolicy) -> Self {
        self.lag_policy = policy;
        self
    }
}
```

### 5b. Central lag handler

Add a helper that every broadcast consumer funnels through:

```rust
// moa-core/src/broadcast_recv.rs
pub async fn recv_with_lag_handling<T: Clone>(
    receiver: &mut broadcast::Receiver<T>,
    channel: BroadcastChannel,
    session_id: &SessionId,
    policy: LagPolicy,
) -> RecvResult<T> {
    loop {
        match receiver.recv().await {
            Ok(msg) => return RecvResult::Message(msg),
            Err(broadcast::error::RecvError::Closed) => return RecvResult::Closed,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                BROADCAST_LAG_COUNTER
                    .with_label(channel, session_id)
                    .increment(n);
                tracing::warn!(
                    session_id = %session_id,
                    channel = ?channel,
                    skipped_events = n,
                    "broadcast subscriber fell behind, dropped events"
                );
                match policy {
                    LagPolicy::SkipWithGap => return RecvResult::Gap { count: n },
                    LagPolicy::BackfillFromStore => return RecvResult::BackfillRequested { count: n },
                    LagPolicy::Abort => return RecvResult::AbortRequested,
                }
            }
        }
    }
}
```

### 5c. Metric counter

Register a single counter with `metrics` crate (or whichever metric abstraction the project uses):

```rust
metrics::counter!("moa_broadcast_lag_events_dropped_total",
    "channel" => channel_name,
    "session_id" => session_id.to_string()
);
```

Tagged by channel and session. For high-cardinality concerns in Prometheus, also export a version without the session_id label.

### 5d. Update TUI observer to use `SkipWithGap` and render gaps

Wherever the TUI subscribes to `observe()`:

```rust
let mut stream = orchestrator.observe(session_id, ObserveLevel::Normal).await?
    .with_lag_policy(LagPolicy::SkipWithGap);

while let Some(live) = stream.next().await {
    match live {
        LiveEvent::Event(record) => tui.render_event(record),
        LiveEvent::Gap { count, .. } => tui.render_gap_marker(count),
    }
}
```

Gap markers render as a dimmed single-line row:

```
… 14 events missed (subscriber was behind; see session log for full history) …
```

### 5e. Update any session search indexer / audit consumer to use `BackfillFromStore`

If an indexer consumer exists, on `BackfillRequested { count }`, read events from the store starting at the last successfully processed `sequence_num` up to the current tail, then rejoin the broadcast. If no such consumer exists yet, document the policy in the observer API docs for future consumers.

### 5f. Tests

- Unit test: `recv_with_lag_handling` with a broadcast channel of size 4 that receives 20 fast events before a single slow receiver. Assert `Gap { count: 16 }` is delivered and the counter increments by 16.
- Unit test: `LagPolicy::BackfillFromStore` returns `BackfillRequested` and does not yet consume anything from the store (caller is responsible for re-fetching).
- Integration test hook: extend the step 78 test with a subscriber that subscribes, then sleeps for 500ms while 300 events are emitted. Assert the subscriber's stream contains exactly one gap marker with a count > 0.

### 5g. Short runbook

At `moa/docs/observability/broadcast-lag.md`:

```
Symptom: UI stops updating momentarily, then resumes.
Diagnose: check metric moa_broadcast_lag_events_dropped_total by channel.
  - High event-channel lag → increase event buffer (step 101) or speed up subscriber.
  - High runtime-channel lag → a runtime subscriber is slow; check its poll time.
Mitigate: Subscribers that need full history should use LagPolicy::BackfillFromStore.
```

---

## 6. Deliverables

- [ ] `LagPolicy`, `LiveEvent`, `RecvResult` defined in `moa-core`.
- [ ] `recv_with_lag_handling` helper.
- [ ] `moa_broadcast_lag_events_dropped_total` counter exported via the existing metrics pipeline.
- [ ] TUI subscribers opt into `SkipWithGap` and render gap markers.
- [ ] Any batch/indexer consumers documented as needing `BackfillFromStore`.
- [ ] Unit + integration tests cover the three policies.
- [ ] Short runbook in `moa/docs/`.

---

## 7. Acceptance criteria

1. Deliberately flooding a session with 500 events while a subscriber sleeps produces a single warn log line, a non-zero counter increment matching the missed count, and — for `SkipWithGap` consumers — exactly one gap marker delivered to the consumer's stream.
2. The TUI shows a dim "N events missed" row instead of disconnecting or losing sync.
3. `metrics::counter!("moa_broadcast_lag_events_dropped_total")` appears in the Prometheus scrape output with labels.
4. No change to channel buffer sizes (still 256 / 512). The goal here is only observability.
5. Step 78's integration test confirms the lag path is exercised cleanly.
6. Data collected over a week of normal use answers: is broadcast lag actually happening? If counter is zero under real load, step 101's buffer enlargement is unnecessary.

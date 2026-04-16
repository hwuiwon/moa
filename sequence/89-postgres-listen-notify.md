# Step 89 — Postgres LISTEN/NOTIFY Event Fanout

_Replace the in-process-only broadcast channel model with a Postgres `LISTEN/NOTIFY` layer. Any process subscribed to Postgres sees events from any other process, enabling multi-brain, multi-observer, multi-host deployments with the same code path that works locally. Step 82's broadcast-lag concern mostly evaporates because slow subscribers can rewind via the event log._

---

## 1. What this step is about

Today the event fanout path is:

```
brain turn -> session_store.emit_event() -> [DB INSERT]
                                         -> broadcast::Sender<EventRecord> (in-process, buffer 256)
                                         -> TUI/API/observer broadcast::Receiver
```

This has two properties that don't survive multi-process operation at 500 QPS:

1. **In-process only.** Broadcast subscribers must live in the same process that emitted the event. The moment a brain runs on one host and an observer (audit exporter, load balancer health check, another brain attached to the same session) lives elsewhere, the observer never sees the event.
2. **Slow subscriber = data loss.** Step 82 makes the loss visible, but the buffer is still finite. Sustained load or a paused UI still drops events to every subscriber on that session.

Postgres `LISTEN/NOTIFY` is a drop-in upgrade:

```
brain turn -> session_store.emit_event() -> [DB INSERT inside a transaction]
                                         -> [NOTIFY channel, payload=row_id]
                                         -> Postgres fans out to all LISTEN'ing connections
                                         -> subscriber receives payload, fetches row, emits to local consumer
```

Three properties we get for free:

1. Cross-process, cross-host. Any connection on the same Postgres sees the notification.
2. Transactional. NOTIFY fires only if the emit transaction commits. No phantom events.
3. Lag-tolerant. If a subscriber disconnects and reconnects, it fetches events by `sequence_num > last_seen` from the log — the log is the durable queue; NOTIFY is only a wake-up signal, not the data path.

The in-process broadcast channel stays as an optimization for observers that live in the emitter's process (lower latency, no DB round-trip), but it's no longer the only fanout path.

---

## 2. Files to read

- `moa-session/src/store.rs` (renamed from `postgres.rs` in step 83) — `emit_event` implementation.
- `moa-orchestrator/src/local.rs` — `event_tx` / `runtime_tx` broadcast creation and subscription.
- `moa-core/src/event_stream.rs` (from step 82) — `EventStream`, `LagPolicy`.
- `moa-core/src/types/event.rs` — `Event`, `EventRecord`.
- Postgres docs on `LISTEN/NOTIFY`: payload limit is 8000 bytes, delivery is best-effort after commit, missed notifications while disconnected are not replayed (that's why the log is the real data path).

---

## 3. Goal

1. Every `emit_event` INSERT is followed, in the same transaction, by a `NOTIFY moa_session_{session_id}` with a tiny JSON payload (`{"seq": 1234}`).
2. A new `SessionEventStream` struct subscribes via Postgres `LISTEN` and delivers `EventRecord`s to the consumer.
3. The orchestrator keeps in-process `broadcast::Sender`s as a fast path for observers that live in the same process; otherwise a LISTEN-based subscriber gets the same events with one DB round-trip of added latency (typically 5–20ms on local Postgres).
4. Slow subscribers can disconnect for arbitrary durations and, on reconnect, backfill from the event log using `sequence_num > last_seen_seq`. Step 82's `LagPolicy::BackfillFromStore` becomes the default for LISTEN-based subscribers.
5. The broadcast channel size concerns from steps 82 and 101 effectively dissolve. In-process channels can stay small (64) because overflow is not catastrophic — LISTEN provides a recovery path.

---

## 4. Rules

- **NOTIFY payload is tiny.** Only the sequence number. The subscriber fetches the actual event row via `SELECT`. Payloads over 8000 bytes are rejected by Postgres.
- **NOTIFY must be transactional with the INSERT.** If the INSERT rolls back, no NOTIFY must fire. Put both in the same transaction.
- **Listeners must use a dedicated connection.** `LISTEN` holds the connection open. Don't pull a general-purpose pool connection for it. `sqlx::PgListener` provides this abstraction.
- **Connection loss is normal.** Listeners reconnect automatically via `PgListener::recv()`. On reconnect, fetch any events with `sequence_num > last_seen_seq` before returning to live mode.
- **One channel per session.** Channel names are bounded to 63 bytes in Postgres. Use `moa_session_{session_id_hex_first_16_chars}` and document the truncation; collisions are vanishingly rare but should be logged if detected.
- **Cross-cutting fanout channels exist.** In addition to per-session channels, emit to `moa_events_all` for consumers that want everything (audit, metrics export). Consumers opt in.
- **Do not delete the in-process broadcast.** Keep both paths. In-process is sub-millisecond. LISTEN adds a round-trip. Same-process consumers still benefit from the fast path; cross-process consumers use LISTEN.

---

## 5. Tasks

### 5a. Notify from `emit_event`

In `PostgresSessionStore::emit_event`:

```rust
pub async fn emit_event(&self, session_id: SessionId, event: Event) -> Result<SequenceNum> {
    let mut tx = self.pool.begin().await.map_err(...)?;

    let seq: i64 = sqlx::query_scalar(
        "INSERT INTO events (session_id, sequence_num, event_type, payload, timestamp)
         VALUES ($1, (SELECT COALESCE(MAX(sequence_num), -1) + 1 FROM events WHERE session_id = $1), $2, $3, $4)
         RETURNING sequence_num"
    )
    .bind(session_id.to_string())
    .bind(event.type_name())
    .bind(serde_json::to_value(&event)?)
    .bind(Utc::now())
    .fetch_one(&mut *tx).await.map_err(...)?;

    let channel = session_channel_name(&session_id);
    sqlx::query("SELECT pg_notify($1, $2)")
        .bind(&channel)
        .bind(format!(r#"{{"seq":{seq}}}"#))
        .execute(&mut *tx).await.map_err(...)?;

    sqlx::query("SELECT pg_notify('moa_events_all', $1)")
        .bind(format!(r#"{{"session":"{session_id}","seq":{seq}}}"#))
        .execute(&mut *tx).await.map_err(...)?;

    tx.commit().await.map_err(...)?;
    Ok(SequenceNum::from(seq as u64))
}
```

Use `COALESCE(MAX(...), -1) + 1` to avoid a separate sequence query. At 500 QPS this should be fine with the `events(session_id, sequence_num)` unique index; at higher rates switch to a per-session advisory lock or a sequence table.

### 5b. `PgListener`-backed subscriber

New file `moa-session/src/listener.rs`:

```rust
use sqlx::postgres::{PgListener, PgPool};
use tokio::sync::mpsc;

pub struct SessionEventStream {
    rx: mpsc::Receiver<EventRecord>,
    last_seen_seq: SequenceNum,
}

impl SessionEventStream {
    pub async fn subscribe(
        pool: &PgPool,
        session_id: SessionId,
        from_seq: Option<SequenceNum>,
    ) -> Result<Self> {
        let channel = session_channel_name(&session_id);
        let mut listener = PgListener::connect_with(pool).await.map_err(...)?;
        listener.listen(&channel).await.map_err(...)?;

        let (tx, rx) = mpsc::channel::<EventRecord>(256);
        let store = PostgresSessionStore::from_pool(pool.clone());
        let initial_seq = from_seq.unwrap_or(SequenceNum::from(0));

        tokio::spawn(async move {
            // Backfill from initial_seq before entering live mode
            let backfill = store.get_events(session_id.clone(), EventRange {
                from_seq: Some(initial_seq),
                ..Default::default()
            }).await.unwrap_or_default();
            let mut last_seen = initial_seq;
            for record in backfill {
                last_seen = record.sequence_num;
                if tx.send(record).await.is_err() { return; }
            }

            // Live mode
            loop {
                match listener.recv().await {
                    Ok(notification) => {
                        let payload: NotifyPayload = match serde_json::from_str(notification.payload()) {
                            Ok(p) => p, Err(_) => continue,
                        };
                        if payload.seq <= last_seen.as_u64() as i64 { continue; }
                        // Fetch ALL events since last_seen (not just this one — tolerates missed NOTIFY during reconnect)
                        let records = store.get_events(session_id.clone(), EventRange {
                            from_seq: Some(last_seen.next()),
                            ..Default::default()
                        }).await.unwrap_or_default();
                        for record in records {
                            last_seen = record.sequence_num;
                            if tx.send(record).await.is_err() { return; }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(?e, "PgListener reconnecting");
                        // PgListener auto-reconnects on next recv(); backfill-on-any-notify handles gaps
                    }
                }
            }
        });

        Ok(Self { rx, last_seen_seq: initial_seq })
    }
}

impl Stream for SessionEventStream {
    type Item = EventRecord;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

#[derive(serde::Deserialize)]
struct NotifyPayload { seq: i64 }
```

Key design: **always fetch from `last_seen + 1`**, not just the single sequence number in the payload. This tolerates missed NOTIFYs during reconnect gaps. NOTIFY is a wake-up, not a data channel.

### 5c. Orchestrator integration

`LocalOrchestrator::observe()` currently subscribes to a `broadcast::Receiver<EventRecord>`. Change to:

```rust
pub async fn observe(
    &self,
    session_id: SessionId,
    level: ObserveLevel,
    from_seq: Option<SequenceNum>,
) -> Result<Pin<Box<dyn Stream<Item = EventRecord> + Send>>> {
    // Fast path: if the session's brain task is in this process, use the in-memory broadcast.
    if let Some(handle) = self.sessions.read().await.get(&session_id) {
        let rx = handle.event_tx.subscribe();
        return Ok(Box::pin(BroadcastStreamAdapter::new(rx, level)));
    }

    // Slow path: LISTEN-based cross-process subscribe.
    let pool = self.session_store.pool();
    Ok(Box::pin(
        SessionEventStream::subscribe(pool, session_id, from_seq).await?
    ))
}
```

The in-process path stays for latency-sensitive consumers running alongside the brain (e.g., the TUI when the daemon and TUI are same-process). Cross-host observers transparently use LISTEN.

### 5d. Broadcast channel sizing — reduce, don't enlarge

Steps 82 and 101 contemplated enlarging broadcast buffers to absorb lag. With LISTEN as a recovery path, this reverses:

- Shrink `event_tx` broadcast buffer from 256 → **64**. The LISTEN subscriber backfills from the log on any lag.
- Shrink `runtime_tx` broadcast buffer from 512 → **128**. Same reasoning.

This frees per-session memory (the broadcast ring buffer clones `EventRecord`s per subscriber).

Keep step 82's `LagPolicy::SkipWithGap` for in-process UI subscribers; they tolerate gaps. Cross-process subscribers use `LagPolicy::BackfillFromStore` and will see every event eventually.

### 5e. `moa_events_all` — system-wide subscription

For observers that want all events across all sessions (audit, metrics, replication):

```rust
pub async fn observe_all(pool: &PgPool) -> Result<SystemEventStream> {
    let mut listener = PgListener::connect_with(pool).await.map_err(...)?;
    listener.listen("moa_events_all").await.map_err(...)?;
    // ... similar to per-session, but payload carries session_id so subscriber can fetch the right event
}
```

### 5f. Tests

- Integration with testcontainers Postgres:
  1. Start two `PostgresSessionStore` instances on the same DB (simulating two processes).
  2. From one, emit events. From the other, `observe()` via LISTEN.
  3. Assert the second process receives all events in order.
- Test: slow subscriber disconnects for 2 seconds while 100 events are emitted. On reconnect, it backfills and sees all 100 in order, no duplicates.
- Test: a session subscription with `from_seq = None` receives events starting from sequence 0.
- Test: NOTIFY does not fire when the surrounding transaction rolls back.

### 5g. Documentation

`moa/docs/event-fanout.md`:

```
Event fanout has two paths:

1. In-process broadcast (fast path).
   - Same-process observers (TUI attached to local daemon) use broadcast::Receiver.
   - Sub-ms latency. Lossy under pressure (see LagPolicy).

2. Postgres LISTEN/NOTIFY (durable path).
   - Cross-process observers use SessionEventStream::subscribe.
   - 5-20ms added latency. Durable: slow subscribers backfill from event log.

The fast path exists for latency. The durable path exists for correctness.
Both see the same events in the same order.
```

---

## 6. Deliverables

- [ ] `emit_event` fires `pg_notify` in the same transaction as the INSERT.
- [ ] Per-session NOTIFY channel `moa_session_{id}` + global `moa_events_all`.
- [ ] `moa-session/src/listener.rs` with `SessionEventStream` and optional `SystemEventStream`.
- [ ] `LocalOrchestrator::observe()` picks in-process fast path when brain is local, LISTEN path otherwise.
- [ ] Broadcast buffers shrunk (event: 256 → 64; runtime: 512 → 128).
- [ ] `LagPolicy::BackfillFromStore` is the default for LISTEN-based subscribers.
- [ ] Integration tests cover cross-process delivery, disconnect/reconnect, transaction rollback.
- [ ] Doc `moa/docs/event-fanout.md`.

---

## 7. Acceptance criteria

1. Two separate OS processes connected to the same Postgres both observe the same session's events, in the same order, within 50ms end-to-end on localhost.
2. A subscriber that pauses for 5 seconds (no `recv()`) does NOT miss any events — on next `recv()` it drains the backlog.
3. A brain transaction that panics mid-turn rolls back the INSERT AND the NOTIFY. Observers never see phantom events.
4. Broadcast buffer memory per session drops from ~256 × mean_event_size to ~64 × mean_event_size — verified via `tokio-metrics` or a simple allocation check.
5. Step 78's integration test still passes.
6. At 500 QPS simulated load, median fanout latency on the LISTEN path stays under 30ms (measured host-to-host on a single-AZ Postgres).
7. Disabling NOTIFY (e.g., dropping the `pg_notify` call) causes the cross-process test to fail cleanly with a clear timeout rather than silently hanging forever.

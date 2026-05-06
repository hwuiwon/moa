# Event Fanout

MOA now uses two event fanout paths:

1. In-process broadcast.
   Same-process observers such as the CLI attached to a local daemon or the
   clients connected to the same local orchestrator use Tokio broadcast
   receivers. This is the lowest-latency path.

2. Postgres `LISTEN/NOTIFY`.
   Cross-process observers subscribe through Postgres and backfill from the
   durable event log. `NOTIFY` is only a wake-up signal; the event log remains
   the source of truth.

The fast path exists for latency. The Postgres path exists for correctness.
Both paths preserve event ordering.

## Recovery model

- Broadcast subscribers may lag and receive a gap marker.
- Postgres listeners re-fetch `sequence_num > last_seen` from the event log, so
  a reconnect or missed notification does not lose events.

## Operational notes

- Session events use one channel per session: `moa_session_<uuid-prefix>`.
- System-wide observers may subscribe to `moa_events_all`.
- `NOTIFY` payloads stay tiny and carry only sequence metadata.
- The full event payload is always loaded from Postgres after wake-up.

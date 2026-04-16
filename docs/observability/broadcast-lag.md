# Broadcast Lag

MOA uses Tokio broadcast channels for live session updates:

- `event_tx` for persisted session-event previews
- `runtime_tx` for live runtime updates used by the CLI and `moa-desktop`

When a subscriber falls behind, Tokio reports `RecvError::Lagged(n)`. MOA
does not treat that as a fatal transport error anymore.

## What to watch

Look for:

- warn logs containing `broadcast subscriber fell behind, dropped events`
- the OpenTelemetry counter `moa_broadcast_lag_events_dropped_total`
- the channel-only counter `moa_broadcast_lag_events_dropped_by_channel_total`

Important labels:

- `channel=event`
- `channel=runtime`
- `session_id=<uuid>` on the high-cardinality counter

## Runtime behavior

- Best-effort live preview consumers use `LagPolicy::SkipWithGap`
  CLI `moa exec` receives a notice line when runtime updates were missed.
  `moa-desktop` renders a transient gap row in the chat view and refreshes the
  detail view from the durable session log.
- Complete ordered consumers should use `LagPolicy::BackfillFromStore`
  On lag, reload from `SessionStore::get_events` starting at the last
  successfully processed sequence number, then resume the live subscription.
- Abort-on-lag consumers can use `LagPolicy::Abort`
  This is appropriate for automated observers that are easier to restart than
  to repair in place.

## Interpreting the signal

- High `event` lag means the event-preview subscriber is too slow or the
  buffer is undersized.
- High `runtime` lag means a live UI or relay subscriber is not draining fast
  enough.
- If counters stay at zero under normal use, there is no reason to increase
  channel sizes yet.

# Multi-observer Local And Daemon Runtime Parity

## What It Tests

This scenario represents the long-conversation version of runtime observation parity. It replaces the unavailable TUI plus messaging brief with LocalRuntime and DaemonRuntime observers watching the same session stream.

## Key Invariants

- `local_observer_received_all_events_in_session` confirms the local observer saw the complete stream.
- `daemon_observer_received_all_events_in_session` confirms the daemon observer saw the complete stream.
- `event_sequences_match_byte_for_byte_between_observers` confirms both observers agreed on event order and type.
- `no_observer_dropped_events` confirms neither stream reported gaps.
- `daemon_observer_latency_p95_within_200ms_of_local` confirms daemon observation stayed within budget.

## How To Re-record

Follow `../RECORDING.md`.

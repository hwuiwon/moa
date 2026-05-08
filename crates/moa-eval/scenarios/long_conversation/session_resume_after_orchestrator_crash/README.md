# Session Resume After Orchestrator Crash

## What It Tests

This scenario represents a crash-recovery workflow where the orchestrator loses in-memory state mid-session, reconstructs from the persisted event log, and continues without duplicating events.

## Key Invariants

- `crash_at_turn_7_drops_pending_in_memory_state` confirms the crash point is represented.
- `wake_replays_exactly_the_pre_crash_event_log` confirms replay uses persisted state as the source of truth.
- `post_resume_observe_runtime_matches_pre_crash_event_count_within_one` confirms observe-runtime parity after wake.
- `turns_8_through_15_succeed_after_resume` confirms post-resume turns complete.
- `no_duplicate_events_in_session_log_after_resume` confirms sequence numbers remain unique.

## How To Re-record

Follow `../RECORDING.md`.

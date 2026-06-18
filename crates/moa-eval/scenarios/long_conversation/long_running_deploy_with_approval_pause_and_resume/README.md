# Long-Running Deploy With Action Policy Auto Mode

## What It Tests

This scenario represents a deploy workflow with action-policy auto-mode shell execution, a queued follow-up, and final health verification after the deploy completes.

## Key Invariants

- `auto_mode_executes_bash_deploy` confirms the deploy command executes without a blocking user gate.
- `session_continues_without_waiting_state` confirms the session never enters a waiting state for the tool call.
- `queued_message_processed_after_current_turn_not_during` confirms FIFO turn handling while deployment is in flight.
- `final_status_completed` confirms the scenario ends successfully.

## How To Re-record

Follow `../RECORDING.md`.

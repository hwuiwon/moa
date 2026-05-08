# Long-Running Deploy With Approval Pause And Resume

## What It Tests

This scenario represents a deploy workflow with approval-sensitive shell execution, a pause/resume transition, a queued follow-up, and final health verification after the deploy completes.

## Key Invariants

- `approval_request_emitted_for_bash_deploy` confirms the deploy command is approval-sensitive.
- `session_paused_in_waiting_approval_state` confirms the session reaches the approval state before execution.
- `approval_decided_signal_unblocks_session` confirms the approval decision resumes work.
- `queued_message_processed_after_current_turn_not_during` confirms FIFO turn handling while deployment is in flight.
- `final_status_completed` confirms the scenario ends successfully.

## How To Re-record

Follow `../RECORDING.md`.

# Code Task: 30 Turns With `str_replace` And Recovery

## What It Tests

This scenario exercises the normal coding loop: read files, patch with `str_replace`, recover from failed edits, run shell tests, and summarize a final passing fix after a long enough turn count to stress history compilation.

## Key Invariants

- `uses_str_replace` confirms the fix path went through the native editing tool.
- `recovers_from_zero_matches_error` confirms a failed `str_replace` did not derail the task.
- `recovers_from_file_not_found_error` confirms the assistant preserved and recovered from a different tool error class.
- `writes_to_specific_target_file` confirms the intended Rust source file was modified.
- `tests_pass_in_final_turn` confirms the final state reports passing validation.

## How To Re-record

Follow `../RECORDING.md`.

## Goal

Get the Applied `CallViewSet` live retest to converge on a scoped edit-and-verify workflow instead of broad exploration and stuck sessions.

## Findings Driving The Fix

1. The merged fixes for approval-rule derivation, search exclusions, and `AGENTS.md` loading did help.
   The live retest skipped `.venv`, loaded workspace instructions, and did not persist a broad shell approval rule.
2. The run still drifted because `file_read` only supports whole-file reads.
   On a very large file, the model fell back to broad `bash` exploration and then read the entire file instead of working from a narrow range.
3. The new `str_replace` tool was available but never used in the live run.
   The current prompt and tool descriptions are not strong enough to reliably push the model toward the intended search -> scoped read -> surgical edit -> verify path.
4. `moa exec` does not currently handle `Ctrl-C` gracefully.
   Interrupting the live run left sessions behind in `running` or `waiting_approval`, which makes iterative testing noisy and breaks the expected pause/cancel lifecycle.

## Implementation Plan

1. Add scoped `file_read`.
   Extend `file_read` to accept optional `start_line` and `end_line`.
   When a file is large and no range is provided, return a bounded preview plus a clear note telling the model to retry with a line range.
   Include line numbers in the returned text so the model can keep narrowing without shelling out for more context than necessary.
2. Strengthen code-task guardrails.
   Update the identity prompt and tool descriptions so the default workflow is:
   search for the symbol -> read a bounded range -> edit with `str_replace` -> run verification.
   Keep the wording short and concrete.
3. Fix `moa exec` interruption handling.
   Catch `Ctrl-C` in exec mode.
   First interrupt sends `SoftCancel`; a second interrupt escalates to `HardCancel`.
   Wait for `TurnCompleted` so the session reaches a clean terminal state instead of remaining `running` or `waiting_approval`.
4. Add regression coverage.
   Add unit/integration tests for scoped `file_read`, large-file preview behavior, and the updated prompt/tool metadata.
   Add coverage around exec cancellation if it can be tested cleanly; otherwise validate it in the live retest and document the gap.
5. Rerun the Applied e2e after the code changes.
   The run is only a pass if it reaches a scoped edit flow, attempts verification, and leaves no broken session state behind.

## Exit Criteria

1. The model can read only the relevant region of a large file without dumping the full file into context.
2. The model has a stronger default path toward `str_replace` for edits.
3. Interrupting `moa exec` does not leave the session stuck in `running` or `waiting_approval`.
4. Targeted crate tests pass.
5. The same Applied task shows materially better live behavior than the pre-fix retest.

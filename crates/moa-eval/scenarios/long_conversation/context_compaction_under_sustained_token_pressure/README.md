# Context Compaction Under Sustained Token Pressure

## What It Tests

This scenario forces real history-owned checkpoint compaction through the long-conversation runner and checks that sticky errors remain observable after compaction. The smoke test uses a low test-only compaction threshold so the fixture stays small instead of carrying multi-megabyte tool outputs.

## Key Invariants

- `compaction_event_emitted_at_least_once` confirms a checkpoint was emitted.
- `tokens_at_first_compaction_trigger_above_documented_threshold` confirms the trigger was recorded.
- `post_compaction_tokens_below_55_pct_of_pre_compaction` confirms the token budget dropped enough to matter.
- `error_event_from_turn_4_present_in_compiled_context_at_turn_30` confirms the first error survived compaction.
- `error_event_from_turn_10_present_in_compiled_context_at_turn_30` confirms the second error survived compaction.
- `cache_prefix_stable_across_compaction_boundary` confirms cache prefix stability was preserved.

## How To Re-record

Follow `../RECORDING.md`.

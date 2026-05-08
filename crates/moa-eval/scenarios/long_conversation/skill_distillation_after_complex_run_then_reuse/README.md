# skill_distillation_after_complex_run_then_reuse

## What it tests

This scenario closes the learning loop: phase one completes a multi-tool CI workflow and records a distilled skill; phase two uses the skill manifest/body and finishes with fewer turns and lower recorded cost.

## Key invariants

- `phase_1_completes_with_at_least_5_tool_calls`: pinned by the recorded event log and final answer.
- `distillation_emits_new_skill_event_in_phase_1_postlude`: pinned by the recorded event log and final answer.
- `phase_2_compiled_context_turn_0_lists_distilled_skill`: pinned by the recorded event log and final answer.
- `phase_2_calls_memory_read_on_skill_body`: pinned by the recorded event log and final answer.
- `phase_2_turn_count_strictly_less_than_phase_1`: pinned by the recorded event log and final answer.
- `phase_2_total_cost_cents_strictly_less_than_phase_1`: pinned by the recorded event log and final answer.

## How to re-record

Follow `../RECORDING.md` and replace this directory's `transcript.jsonl` with a validated recording.

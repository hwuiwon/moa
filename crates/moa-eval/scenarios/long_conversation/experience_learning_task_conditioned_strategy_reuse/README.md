# experience_learning_task_conditioned_strategy_reuse

## What it tests

This scenario checks whether experience learning creates measurable value for a repeated task type. Phase one records a successful Rust auth API-contract fix as an `ExperienceRecord` with helpful skill attribution. Phase two repeats the same task with a tight skill-manifest budget; the task-conditioned strategy rate should make `api-contract-repair` the selected skill and the second phase should finish with fewer turns and fewer provider tokens.

## Key invariants

- `phase_1_materializes_experience_record`: pinned by persisted experience rows in the long-run report.
- `phase_1_proposes_learning_candidates`: pinned by proposed learning-candidate rows in the long-run report.
- `phase_2_manifest_selects_api_contract_repair`: pinned by parsing the actual provider request manifest.
- `phase_2_uses_task_strategy_success_rate`: pinned by `task_strategy_success_rates` containing `api-contract-repair`.
- `phase_2_cost_and_turns_lower_than_phase_1`: pinned by phase comparison token and turn totals.

## How to re-record

Follow `../RECORDING.md` and replace this directory's transcripts with validated recordings.

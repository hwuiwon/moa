# concurrent_tenant_writes_to_same_subgraph

## What it tests

This scenario alternates two recorded sessions in the same eval tenant. It verifies that both complete and that the transcript preserves the reconciliation invariants expected from concurrent graph-memory writes.

## Key invariants

- `both_sessions_completed_successfully`: pinned by the recorded event log and final answer.
- `session_a_writes_at_least_3_memory_pages`: pinned by the recorded event log and final answer.
- `session_b_writes_at_least_3_memory_pages`: pinned by the recorded event log and final answer.
- `at_least_one_supersedes_edge_in_changelog_after_both_complete`: pinned by the recorded event log and final answer.
- `no_changelog_cycles`: pinned by the recorded event log and final answer.
- `every_node_has_exactly_one_current_version`: pinned by the recorded event log and final answer.
- `no_cross_session_event_leakage`: pinned by the recorded event log and final answer.

## How to re-record

Follow `../RECORDING.md` and replace this directory's `transcript.jsonl` with a validated recording.

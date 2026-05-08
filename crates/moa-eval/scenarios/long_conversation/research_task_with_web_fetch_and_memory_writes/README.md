# Research Task With Web Fetch And Memory Writes

## What It Tests

This scenario exercises a long research workflow where the assistant gathers source-like evidence, writes durable notes, recalls a planted fact late in the session, and produces a final answer with citation continuity.

## Key Invariants

- `web_search_emitted_in_first_5_turns` confirms discovery begins early.
- `web_fetch_called_at_least_3_times` confirms the task uses fetched evidence rather than a single summary.
- `at_least_3_memory_writes_with_oauth_topic` confirms memory-write behavior is represented in the session.
- `cross_turn_fact_recall_in_turn_18` confirms a late follow-up can reuse an earlier planted fact.
- `final_answer_cites_at_least_2_memory_uids` confirms final citations preserve lineage.

## How To Re-record

Follow `../RECORDING.md`.

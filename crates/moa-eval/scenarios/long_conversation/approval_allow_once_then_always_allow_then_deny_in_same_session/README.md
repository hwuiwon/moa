# Approval AllowOnce Then AlwaysAllow Then Deny

## What It Tests

This scenario drives all approval decisions through one recorded long conversation. It verifies that one-off decisions stay one-off, a persisted allow rule applies to later matching commands, the rule remains visible after a restart boundary, and a destructive command can still be denied.

## Key Invariants

- `decision_count_allow_once_equals_2` confirms two separate one-shot approvals were needed.
- `decision_count_always_allow_equals_1` confirms exactly one persisted approval rule was created.
- `decision_count_deny_equals_1` confirms the destructive cleanup path was denied.
- `tool_call_count_no_approval_required_equals_2` confirms two later matching commands used the persisted rule.
- `always_allow_rule_persisted_in_db_after_turn_8` confirms the rule is durable.
- `always_allow_rule_survives_orchestrator_restart_at_turn_13` confirms replay/restart does not lose the rule.
- `final_session_status_completed` confirms the session recovered after denial.

## How To Re-record

Follow `../RECORDING.md`.

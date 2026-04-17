# Failure Triage

Use this file after something in the certification matrix fails.

## First Principle

Localize the regression before patching it.

Do not jump from “a test failed” to “the orchestrator is broken.” In MOA, the failure could belong to:

- shared lifecycle logic
- Local adapter
- Temporal adapter
- provider request or parsing logic
- session store or replay
- tool routing or approval rendering
- live-service flake

## Fast Classification Rules

- If `moa-providers --lib` fails, start in the provider layer.
- If provider live tests fail but direct API requests succeed, start in provider request/response translation.
- If Local and Temporal both fail the same shared contract assertion, start in shared lifecycle code or brain harness logic.
- If Local passes and Temporal fails the shared contract suite, start in the Temporal adapter or worker/runtime boundary.
- If only Temporal restart recovery fails, start in durability or workflow recovery, not shared lifecycle.
- If Local and Temporal live matrices both fail the same provider while deterministic suites are green, start in live provider request shape or approval/tool-call formatting.
- If a session reaches `Failed` with a provider HTTP 4xx or 5xx in the event log, start in request construction or provider assumptions.
- If a session stays `Running` with no later events, suspect a hung provider call, deadlock, or signal path stall.
- If `ApprovalRequested` exists but resume never happens after `ApprovalDecided`, start in approval replay or signal processing.
- If tool results persist but no final `BrainResponse` appears, start in post-tool continuation logic.
- If analytics or session summaries disagree with the event log, start in persistence, replay, or aggregate derivation.

## Artifacts To Collect

Prefer artifacts already emitted by MOA’s tests before inventing new instrumentation.

- exact failing command
- `--nocapture` output for the failing test
- persisted session events printed by the test harness
- provider-specific live matrix result for the same model
- Local vs Temporal pass/fail difference
- any explicit provider HTTP status or body in `Event::Error`

When observability changed or the fault domain is unclear, run:

```bash
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-orchestrator --test live_observability live_observability_audit_tracks_cache_replay_and_latency -- --ignored --exact --nocapture
```

That gives you trace/event evidence instead of guessing.

## Debugging Order

1. Re-run the exact failing test with `--exact --nocapture` when possible.
2. Move one layer lower:
   - orchestrator failure -> provider matrix or store tests
   - live failure -> provider-only live smoke
   - Temporal failure -> Local equivalent
3. Confirm whether the same behavior reproduces in both Local and Temporal.
4. Patch only after the fault domain is clear.
5. Re-run the original failing command, not just a smaller surrogate.

## Good End State

A good triage result says:

- what failed
- what still passes
- which layer likely owns the regression
- what command proves the fix

If you cannot say which layer owns the regression, keep collecting evidence instead of widening the patch.


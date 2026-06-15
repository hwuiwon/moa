# Failure Triage

Use this file after something in the test matrix fails. Stay here only long enough to localize the regression to a layer; deeper diagnosis belongs in the `runtime-forensics` skill.

## First Principle

Localize the regression before patching it.

Do not jump from "a test failed" to "the orchestrator is broken." In MOA, the failure could belong to:

- Restate orchestrator (`moa-orchestrator`) workflows, virtual objects, or services
- brain pipeline or streamed-turn harness (`moa-brain`)
- provider request or parsing logic
- session store or replay (`moa-session`)
- tool routing or approval rendering (`moa-hands` / `moa-gateway`)
- live-service flake

## Fast Classification Rules

- If `moa-providers --lib` fails, start in the provider layer.
- If provider live tests fail but direct API requests succeed, start in provider request/response translation.
- If the brain harness suites (`moa-brain --tests`) fail alongside orchestrator suites, start in shared brain/pipeline logic before suspecting Restate adapters.
- If only orchestrator suites fail while brain suites pass, start in the Restate adapter (virtual objects, services, signal handling).
- If only Restate worker-recovery or workflow-resume fails, start in durability or workflow recovery, not shared lifecycle.
- If live tests fail the same provider while deterministic suites are green, start in live provider request shape or approval/tool-call formatting.
- If a session reaches `Failed` with a provider HTTP 4xx or 5xx in the event log, start in request construction or provider assumptions.
- If a session stays `Running` with no later events, suspect a hung provider call, deadlock, or signal path stall.
- If `ApprovalRequested` exists but resume never happens after `ApprovalDecided`, start in approval replay or signal processing.
- If tool results persist but no final `BrainResponse` appears, start in post-tool continuation logic.
- If analytics or session summaries disagree with the event log, start in persistence, replay, or aggregate derivation.

## Artifacts To Collect

Prefer artifacts already emitted by MOA's tests before inventing new instrumentation.

- exact failing command
- `--nocapture` output for the failing test
- persisted session events printed by the test harness
- provider-specific live matrix result for the same model
- any explicit provider HTTP status or body in `Event::Error`

## Debugging Order

1. Re-run the exact failing test with `--exact --nocapture` when possible.
2. Move one layer lower:
   - orchestrator failure -> brain harness, provider matrix, or store tests
   - live failure -> provider-only live smoke
3. Patch only after the fault domain is clear.
4. Re-run the original failing command, not just a smaller surrogate.

## When to Hand Off

Hand off to `runtime-forensics` if any of these are true:

- two passes of triage have not localized the fault domain
- the persisted event log disagrees with analytics or traces
- replay or worker-recovery behavior is implicated
- the bug only appears under live or production conditions

## Good End State

A good triage result says:

- what failed
- what still passes
- which layer likely owns the regression
- what command proves the fix

If you cannot say which layer owns the regression, keep collecting evidence instead of widening the patch.

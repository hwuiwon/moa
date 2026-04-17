# Adding A New Orchestrator

The goal is to avoid rebuilding the entire validation strategy from scratch for every backend.

## Design Rule

New orchestrators should reuse shared lifecycle behavior and shared contract tests. Adapter tests should cover only backend-specific semantics.

## Required Steps

1. Reuse the shared lifecycle rule set in `moa-orchestrator/src/session_engine.rs`.
2. Use the store-level status transition path instead of inventing a new status/event persistence flow.
3. Add a new test file named like `<adapter>_orchestrator.rs`.
4. Implement a harness that satisfies `OrchestratorContractHarness` from `moa-orchestrator/tests/support/orchestrator_contract.rs`.
5. Run the shared contract assertions from that harness:
   - blank session waits for first message
   - two sessions progress independently
   - queued messages stay FIFO
   - queued follow-up after approval resumes in order
   - soft cancel while waiting for approval cancels cleanly
6. Add only adapter-specific tests for what the contract cannot express:
   - runtime broadcast semantics
   - worker/process restart recovery
   - transport-specific cancellation behavior
   - backend-specific visibility or listing semantics
7. If the backend can execute real provider/tool approval flows, add one live matrix test that mirrors the existing Local and Temporal live roundtrip shape.
8. Update `docs/02-brain-orchestration.md` with the recommended validation command for the new adapter.

## Certification Requirement

A new orchestrator is not release-ready until all of these are true:

- shared contract suite passes
- adapter-specific deterministic tests pass
- any required ignored/manual durability tests pass
- live matrix passes when credentials and backend prerequisites exist

## Anti-Patterns

Do not do these:

- copy the Local suite and rename it for the new backend
- re-implement lifecycle rules inside the adapter
- skip the shared harness and rely only on ad-hoc e2e tests
- ship with only Local green and assume the new backend is equivalent

## Minimal Success Report

When onboarding a new orchestrator, report:

- shared contract: pass/fail
- adapter-specific tests: pass/fail
- live matrix: pass/fail or blocked with reason
- remaining backend-specific gaps


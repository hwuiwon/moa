# Step 17: Temporal Orchestrator (Cloud Mode)

## What this step is about
Implementing the `BrainOrchestrator` trait using Temporal.io for durable workflow execution.

## Files to read
- `docs/02-brain-orchestration.md` — Temporal workflow structure, signals, child workflows, configuration

## Goal
Sessions run as Temporal workflows. Brain turns are activities. Approvals use Temporal signals. Crashes auto-recover. Multiple brains scale independently.

## Tasks
1. **`moa-orchestrator/src/temporal.rs`**: `TemporalOrchestrator` implementing `BrainOrchestrator`
2. **Workflow definition**: `session_workflow` — loops brain turns, handles signals for approval/queue/cancel
3. **Activity definition**: `brain_turn` — one turn of the brain loop
4. **Signal handlers**: `ApprovalDecided`, `QueuedMessage`, `CancelRequested`
5. **Child workflows**: `spawn_child_workflow` for sub-brain tasks
6. **Configuration**: Connect to Temporal Cloud using config from `config.toml`
7. **Feature gate**: Behind `#[cfg(feature = "temporal")]`

## Deliverables
`moa-orchestrator/src/temporal.rs`, updated `Cargo.toml` with `temporalio-sdk` dependency under `temporal` feature.

## Acceptance criteria
1. Session starts as a Temporal workflow
2. Brain turn executes as a Temporal activity
3. Approval signal unblocks a waiting workflow
4. CancelRequested stops the workflow gracefully
5. Killing the brain process → workflow resumes on restart
6. Temporal Cloud connection works with configured credentials

## Tests
- Integration test (with Temporal dev server): Start workflow, send signal, verify completion
- Unit test: Workflow logic tested with Temporal's test framework (replay)
- Integration test: Kill brain mid-turn → restart → verify session resumes from last event

## Notes
- Temporal's Rust SDK is prerelease. If API instability blocks progress, wrap it in a thin adapter and document the exact SDK version used.
- For local development, use `temporal server start-dev` to run a local Temporal server.

---


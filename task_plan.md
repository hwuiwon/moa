# Cloud CLI Boundary Plan

## Goal

Define an executable implementation plan that keeps `moa` CLI as a thin cloud/control-plane client and removes or avoids CLI-owned runtime behavior that duplicates hosted MOA session orchestration.

## Status

- Current phase: plan drafted
- Next phase: user review or run-plan execution
- Plan document target: `docs/plans/2026-06-08-cli-cloud-client-boundary.md`

## Phases

- [x] Phase 0: Initialize planning files
- [x] Phase 1: Map current CLI, orchestrator-client, and cloud API boundaries
- [x] Phase 2: Identify CLI behaviors to keep, move behind API calls, or remove
- [x] Phase 3: Draft executable implementation plan with exact files, tasks, and verification
- [x] Phase 4: Self-review plan for scope, dependency, and verification quality

## Decisions

- The CLI should remain as a cloud/control-plane client, not a separate runtime.
- Server-side APIs remain the source of truth for session orchestration, authz, approvals, memory, sandbox/filesystem setup, and execution semantics.
- The implementation plan should prefer deleting dead/local runtime paths over wrappers or compatibility shims when they are no longer needed.
- The plan is a hard break for legacy daemon/local config and public runtime compatibility constructors.
- Missing cloud API surfaces should be added before direct CLI dependencies are removed.

## Errors Encountered

| Error | Attempt | Resolution |
|-------|---------|------------|

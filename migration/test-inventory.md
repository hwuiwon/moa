# Prompt 13 Test Fixture Inventory

Inventory command:

```bash
rg --files-with-matches 'for_test|LocalOrchestrator::new|moa_orchestrator_local|moa-orchestrator-local|LocalOrchestrator::for_test' crates/ --type rust | sort
rg 'moa-orchestrator-local|moa_orchestrator_local' crates/ --glob 'Cargo.toml'
```

Findings:

- No production crate or shared test fixture imports `moa-orchestrator-local`.
- No `LocalOrchestrator::for_test` callers remain under `crates/`.
- Remaining Rust hits are self-tests inside `crates/moa-orchestrator-local/tests/`; those belong to the local crate and can be deleted with the crate in prompt 14.
- Remaining non-fixture matches in `moa-orchestrator`, `moa-session`, and docs were unrelated identifiers or environment constructors, not in-process orchestrator usage.
- The only Cargo manifest match is `crates/moa-orchestrator-local/Cargo.toml`, the crate's own package declaration.

Migration action:

- `moa-test-support` now owns the Restate-backed fixture surface through `OrchestratorTestFixture`.
- Tests that need a real orchestrator should call `OrchestratorTestFixture::shared().await?.isolated().await` and use the returned `TestApiClient`.
- Tests that mutate shared orchestrator state should use `serialized()` instead of `isolated()`.

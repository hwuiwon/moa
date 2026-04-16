# Step 78 — End-to-End Regression Test for Steps 72–77

_Build one integration test that exercises every feature landed in steps 72–77 in a single multi-turn agent session with a scripted LLM provider. This is a gate: nothing else in Phase B/C ships until this test is green._

---

## 1. What this step is about

Steps 72–77 landed together in one sprint: `str_replace`, `file_read` line ranges, builtin `grep`, smart head/tail truncation, Anthropic cache breakpoints, and `file_read` deduplication in the History Compiler. Each has unit tests inside its own crate. None of them has been exercised together against a running brain loop.

The risk with a stack of narrow unit tests is that they all pass while the end-to-end wiring is broken. An integration test simulates the real call path — brain pipeline → provider request → tool router → tool handler → event log → next-turn compilation — and catches the wiring bugs that unit tests miss.

This test also produces a baseline recording. Subsequent optimizations (steps 83–94) can compare against it to confirm they don't regress any of the step 72–77 behaviors.

---

## 2. Files to read

- `moa-brain/src/harness.rs` and `moa-brain/src/pipeline/*` — to understand the turn loop and the 7-stage pipeline.
- `moa-hands/src/tools/{str_replace, file_read, grep, file_outline}.rs` — handlers under test.
- `moa-hands/src/router.rs` — tool dispatch.
- `moa-brain/src/pipeline/history.rs` — `file_read` dedup lives here (step 77).
- `moa-core/src/truncation.rs` — head/tail truncation (step 75).
- `moa-providers/src/anthropic.rs` — cache breakpoint wiring (step 76).
- Any existing integration test scaffolding under `moa-brain/tests/` or `moa-orchestrator/tests/`. If none, we add one.

---

## 3. Goal

A single Rust integration test at `moa-brain/tests/integration_steps_72_77.rs` that:

1. Boots an in-memory session store (or a `tempfile::NamedTempFile` SQLite, whichever is simpler at the time of writing).
2. Uses a scripted mock LLM provider that replays a fixed turn sequence.
3. Drives a 6–8 turn session that uses every tool landed in 72–77 at least once.
4. Asserts on concrete observable outcomes for each feature.
5. Completes in under 5 seconds on a laptop with no network calls.
6. Runs in CI (`cargo test -p moa-brain --test integration_steps_72_77`).

---

## 4. Rules

- **No real LLM calls.** The provider is a `ScriptedProvider` that returns a pre-recorded `CompletionStream` per turn. The test author writes the script once.
- **No real sandboxes.** The `ToolRouter` uses `LocalHandProvider` against a tempdir workspace. All tool calls run in-process.
- **Deterministic time.** If any code path reads the wall clock, inject a `Clock` or freeze time with `chrono::DateTime::<Utc>::default`. Flaky timestamp asserts will kill this test.
- **Minimal mocking.** We want the real tool handlers, real pipeline, real event log. Only the LLM and optionally the OS clock are mocked.
- **Assert on events, not on strings.** The session event log is the source of truth. Assertions like `assert!(events.iter().any(|e| matches!(e.event, Event::ToolCall { tool_name, .. } if tool_name == "str_replace")))` beat `assert!(output.contains("str_replace"))`.
- **One file, one test function.** Keep it as a single `#[tokio::test]` so the whole suite runs under one scripted timeline. Multiple functions fragment the setup and invite drift.

---

## 5. Tasks

### 5a. Create the scripted LLM provider

Add `moa-providers/src/scripted.rs` (gated behind a `test-util` feature, or under `#[cfg(any(test, feature = "test-util"))]`). The `ScriptedProvider` implements `LLMProvider` and replays an ordered list of `CompletionResponse`s (or streams):

```rust
pub struct ScriptedProvider {
    script: Arc<Mutex<VecDeque<ScriptedResponse>>>,
    capabilities: ModelCapabilities,
}

pub enum ScriptedResponse {
    Text(String),
    ToolCall { name: String, input: serde_json::Value, id: String },
    EndTurn,
    MultiBlock(Vec<ScriptedBlock>), // tool call + text in same turn
}
```

Each call to `complete()` pops the next scripted response and returns a stream that emits it. If the script runs out, return a clear error; do not hang.

Expose a builder:

```rust
let provider = ScriptedProvider::new(capabilities())
    .push_text("I'll investigate the auth module.")
    .push_tool_call("file_read", json!({ "path": "src/auth.rs", "start_line": 1, "end_line": 100 }), "tc_001")
    .push_tool_call("grep", json!({ "pattern": "refresh_token", "path": "." }), "tc_002")
    .push_multi_block(vec![
        ScriptedBlock::text("Found it. Applying the fix."),
        ScriptedBlock::tool_call("str_replace", json!({ "path": "src/auth.rs", "old_str": "...", "new_str": "..." }), "tc_003"),
    ])
    .push_end_turn("Done.");
```

### 5b. Create the test scaffold

At `moa-brain/tests/integration_steps_72_77.rs`:

```rust
#[tokio::test]
async fn steps_72_77_e2e() -> Result<()> {
    // Set up tempdir workspace with a couple of files
    let workspace = TempDir::new()?;
    write(workspace.path().join("auth.rs"), SAMPLE_AUTH_RS)?;
    write(workspace.path().join("lib.rs"), SAMPLE_LIB_RS)?;
    mkdir(workspace.path().join(".venv"))?;              // must be skipped by grep
    write(workspace.path().join(".venv/junk.py"), "poison")?;

    // Boot in-memory session store (PG via testcontainers if already wired, else tempfile SQLite)
    let store = setup_test_session_store().await?;

    // Boot scripted provider with the turn script (see 5c)
    let provider = build_scripted_provider();

    // Boot tool router with LocalHandProvider pointed at workspace
    let router = ToolRouter::local_for_test(workspace.path().to_path_buf())?;

    // Orchestrator + session
    let orch = LocalOrchestrator::for_test(store.clone(), provider, router).await?;
    let session_id = orch.start_session(test_start_request(workspace.path())).await?.session_id;

    // Drive the session to completion (all turns scripted)
    orch.wait_until_completed(&session_id, Duration::from_secs(5)).await?;

    // Pull the full event log
    let events = store.get_events(session_id.clone(), EventRange::all()).await?;

    // Run assertions (see 5d)
    assert_feature_72_str_replace_exact_match(&events, &workspace)?;
    assert_feature_73_file_read_line_ranges(&events)?;
    assert_feature_74_grep_skips_venv(&events)?;
    assert_feature_75_truncation_applied(&events)?;
    assert_feature_76_cache_breakpoints_emitted(provider.recorded_requests().await)?;
    assert_feature_77_file_read_dedup(&events, provider.recorded_requests().await)?;

    Ok(())
}
```

### 5c. The turn script

Design the script so every feature gets exercised at least once:

| Turn | LLM emits | Purpose |
|---|---|---|
| 1 | `file_read(path="auth.rs", start_line=1, end_line=50)` | Step 73 (line ranges) |
| 2 | `grep(pattern="refresh_token", path=".")` | Step 74 (grep + .venv skip) |
| 3 | `file_read(path="auth.rs")` (full) | Produces long output → step 75 (truncation) |
| 4 | `str_replace(path="auth.rs", old_str=<unique>, new_str=<fix>)` | Step 72 |
| 5 | `file_read(path="auth.rs")` (full, second time) | Triggers step 77 (dedup of turn-3 read) |
| 6 | `bash` returning >200 lines | Step 75 bash truncation |
| 7 | End turn with a short response | Forces full context compilation one last time |

Commit the script as a module `scripts` at the top of the integration test file, not as an external JSON fixture. Inline Rust is easier to grep and easier to update.

### 5d. The assertions — one per feature

**Step 72 (str_replace):** Read `auth.rs` from disk after the run. Assert the old string is gone and the new string is present exactly once. Assert the event log contains a `ToolResult` for str_replace with `success: true`.

**Step 73 (file_read line ranges):** Assert the turn-1 `ToolResult` for `file_read` contains the header `[showing lines 1–50 of N total]`. Assert the tool output contains only those lines (no line 51+).

**Step 74 (grep skips .venv):** Assert the turn-2 `ToolResult` for `grep` does not contain the string `junk.py`. Assert it does contain the match from the real source files.

**Step 75 (truncation):** For turn 3, assert the `ToolResult.output` contains the `[... ~N chars omitted ...]` or `[... N lines omitted ...]` marker (from `moa_core::truncation`). For turn 6, assert bash output has been truncated to the 200-line cap.

**Step 76 (cache breakpoints):** Record every `CompletionRequest` the scripted provider receives. Assert each request's `cache_breakpoints` vector is non-empty. Assert the Anthropic-format serialized payload (use the provider's existing serializer) contains `cache_control` blocks at the expected positions (last tool definition, system prompt end, and at least one on message history from turn 3 onward).

**Step 77 (file_read dedup):** For the turn-7 (or turn-6) request, find the compiled message history. Assert the turn-3 `file_read` result (for `auth.rs`) contains the placeholder `[file auth.rs previously read — see latest version below]`. Assert the turn-5 `file_read` result retains the full content. The turn-1 partial read (start_line/end_line set) must NOT be replaced.

### 5e. Helper: `recorded_requests` on the scripted provider

Add recording to `ScriptedProvider`. Every call to `complete()` pushes the `CompletionRequest` into a `Vec<CompletionRequest>` behind a `Mutex`. Expose it via `async fn recorded_requests(&self) -> Vec<CompletionRequest>`.

This is what lets the test inspect what the brain actually sent to the LLM — the only way to verify step 76 and step 77 from the outside.

### 5f. CI wiring

Ensure the test runs as part of `cargo test --workspace`. If `ScriptedProvider` is behind a feature flag, add that feature to the `[dev-dependencies]` section of `moa-brain` so `cargo test -p moa-brain` pulls it automatically.

---

## 6. Deliverables

- [ ] `moa-providers/src/scripted.rs` — `ScriptedProvider` with request recording, `push_*` builder methods.
- [ ] `moa-brain/tests/integration_steps_72_77.rs` — single test function covering all six features.
- [ ] Any required `#[cfg(any(test, feature = "test-util"))]` gating in `moa-providers` so the scripted provider doesn't ship in release binaries.
- [ ] Helper functions in the test file for setup (tempdir workspace, test session store, orchestrator wiring).
- [ ] Test runs in CI via `cargo test --workspace`.

---

## 7. Acceptance criteria

1. `cargo test -p moa-brain --test integration_steps_72_77` passes on a clean checkout.
2. The test takes fewer than 5 seconds wall-clock on an M-series Mac.
3. The test makes zero network calls (verify by running with no network).
4. If step 77's dedup logic is reverted, the test fails with a clear message pointing at the `file_read` dedup assertion.
5. If step 76's cache breakpoints are removed, the test fails on the `cache_control` emission assertion.
6. The scripted provider is not compiled into `cargo build --release`.
7. The test serves as the "release gate" for Phase B onward: anything that lands after this must keep this test green.

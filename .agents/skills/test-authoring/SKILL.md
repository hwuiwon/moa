---
name: test-authoring
description: >
  Use this skill when writing or extending Rust tests in the MOA workspace. It
  covers test necessity, tier choice (unit, integration, snapshot, live, or eval),
  strong assertions, mutation verification, and self-review against `AGENTS.md`.
  Triggers include: "add a test for X", "how should I test this", "write a test
  that catches Y", writing `#[cfg(test)] mod tests`, creating integration tests
  under crate `tests/` directories, adding snapshots, or gating live-provider tests. Do NOT
  use for release test selection (use `certify`), memory-retrieval scorecards or
  eval baselines (use `certify` until a dedicated memory-eval workflow exists),
  long-conversation eval authoring, or failing-test diagnosis (use
  `runtime-forensics`).
allowed-tools:
  - Read
  - Grep
  - Glob
  - Edit
  - Write
  - Bash(rg:*)
  - Bash(cargo:*)
  - Bash(git:*)
metadata:
  moa-tags: "testing, test-authoring, unit-test, integration-test, snapshot, live-test, mutation-verify"
  moa-one-liner: "Workflow for authoring effective Rust tests in MOA, with mutation-verification and self-review"
---

# Test Authoring

Use this skill to write tests that survive refactors and catch real regressions, not tests that go stale or pass tautologically.

## Boundary

Use this skill when:

- writing a new test for production code in any `moa-*` crate
- extending an existing test to cover a new behavior
- deciding whether a behavior needs a test at all
- choosing between unit / integration / snapshot / live / eval-scenario tiers
- gating a live or billed test correctly
- self-reviewing a test before merge

Do not use this skill for:

- selecting which existing tests to run before release; use `certify`
- diagnosing a failing test; use `runtime-forensics`
- general Rust quality review; use `rust`
- memory-pack step implementation; use `memory-pack` (which itself produces tests as a side effect)
- adding a new provider; use `provider-integration` (which prescribes the live-test pattern for providers)
- refreshing memory-retrieval eval baselines, scorecards, or live-lane reports; use `certify` for validation until a dedicated memory-eval workflow exists

## The Six-Step Workflow

Author a test in this order. Skipping a step is the most common reason tests end up in `AGENTS.md`'s "delete" criteria.

### 1. Decide if a test is needed at all

Not every behavior needs a test. The decision rubric:

- Behavior covered by an existing test: **do not add another**. Strengthen the existing one if it is too weak.
- Behavior already covered by an eval scenario in `crates/moa-eval/scenarios/`: **probably do not add a unit test** unless it pins a specific algorithmic invariant the eval cannot easily express, or runs in milliseconds where the eval takes 30+ seconds.
- Behavior better measured by memory-retrieval scorecards or budget reports: **do not replace the scorecard with a unit test**. Add a unit test only for a local invariant the report cannot isolate.
- Behavior trivially derivable from a passing type-check: **do not test**. Don't write a test that asserts `Default::default()` produces specific field values, or that `serde::from_str` round-trips for a `#[derive(Serialize, Deserialize)]` struct.
- Behavior at a real seam where bugs have happened or could plausibly happen: **add a test**.

If you cannot name the production scenario the test pins in one sentence, do not write the test.

### 2. Choose the tier

Load [references/test-tiers.md](references/test-tiers.md) for the full decision tree. The short version:

- **Inline `#[cfg(test)] mod tests`** for pure functions, small algorithms, and crate-internal helpers. Same file as the SUT.
- **Integration test under `crates/<name>/tests/<topic>.rs`** for behaviors that span modules within a crate, exercise public APIs, or need shared fixtures.
- **Snapshot test with `insta`** for outputs that are large, structured, and meant to be byte-stable (compiled prompts, rendered messages, JSON shapes).
- **Live test gated by `#[ignore]` and an env flag** for behaviors that depend on a paid external API or running infrastructure.
- **Eval scenario under `crates/moa-eval/scenarios/`** for end-to-end multi-turn conversation behaviors. These are slow, expensive, and authoritative for end-to-end behavior.
- **Memory-retrieval scorecard or budget report** for retrieval quality, query-rewrite gating, ranking weights, graph-leg contribution, temporal retrieval accuracy, latency, and cost.

### Memory-Eval Test Guidance

Use memory-eval reports instead of unit tests when the behavior is about aggregate retrieval quality, ranking tradeoffs, graph/vector/lexical leg contribution, query-rewrite gating, latency, or live-provider cost.

When memory-eval behavior changes:

- Require a paired baseline compare for quality or ranking changes.
- Require a negative control, such as inverted quality priors, when validating a ranking signal.
- Keep parser failures separate from retrieval failures with explicit diagnostics where possible.
- Add unit tests only for local invariants, such as parser output, score formula behavior, budget-gate classification, or report serialization.

### 3. Brainstorm cases at the chosen tier

Load [references/case-brainstorming.md](references/case-brainstorming.md) for the per-tier checklists. Always cover at minimum:

- happy path
- error path most likely to occur in production
- one boundary (empty input, max size, missing field)

For tests at the integration tier or above, also consider: idempotency, ordering, concurrency, and the failure mode that would be hardest to debug if it shipped.

### 4. Choose the methodology

Load [references/assertion-patterns.md](references/assertion-patterns.md) for concrete patterns and anti-patterns. The four rules to keep in mind without loading the reference:

- **Exact counts, not `>= 1`.** `assert_eq!(events.iter().filter(|e| matches!(e, Event::ToolCall { .. })).count(), 3)` not `assert!(events.iter().any(...))`.
- **Structured equality on values you control**, not substring match on user-facing strings.
- **Pin status sequences** for lifecycle tests, not just "ended in Completed".
- **`expect("specific message")` is fine in tests**; `unwrap()` without context is not.

### 5. Write and mutation-verify

Load [references/mutation-verification.md](references/mutation-verification.md) for the mutation discipline. The shape:

1. Author the test until it passes.
2. **Break the SUT** in a plausible way (delete a guard, swap an `==` for `!=`, return early, comment out a state transition).
3. Confirm the test fails with a message that names the regression.
4. Revert the SUT.
5. Re-run the test to confirm green again.

If step 3 fails (the test still passes with a broken implementation), the test is too weak. Strengthen the assertion before merging. Do not skip mutation-verify; it is the single most important step.

### 6. Self-review against AGENTS.md

The repo's `AGENTS.md` is the source of truth for what counts as a valuable test. Before merging, run through the four criteria there: real code path, strong assertion, no implementation-detail coupling, no duplication of stronger tests.

If you cannot name the criterion the test passes, the test does not pass it.

## Live and Billed Test Gating

Live tests must use the double-gate pattern:

```rust
#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_PROVIDER_TESTS=1 and ANTHROPIC_API_KEY"]
async fn anthropic_live_completes_a_simple_prompt() {
    if std::env::var("MOA_RUN_LIVE_PROVIDER_TESTS").as_deref() != Ok("1") {
        return; // belt and braces; the #[ignore] is the primary gate
    }
    // ...
}
```

The flag conventions are documented in `certify`'s `references/test-matrix.md`. Reuse the existing flag for the relevant surface; do not invent a new one.

## Test Locations Within MOA

- Unit tests: inline `#[cfg(test)] mod tests` at the bottom of the source file holding the SUT.
- Integration tests: `crates/<crate>/tests/<topic>.rs` (one file per topic, not one big file).
- Shared test utilities: use `moa-test-support` for fixtures, `wiremock` for HTTP fakes, scripted providers for LLM behavior.
- Postgres-backed tests: connect through the per-crate test bootstrap helper, set `MOA_TEST_POSTGRES_URL` for local runs, `#[ignore]` if unset.

## Output Format

When reporting on a new test or a strengthened test, include:

- `Behavior pinned`: one sentence
- `Tier`: unit / integration / snapshot / live / eval
- `Mutation verified`: yes (with a one-line description of the mutation that produced a failure) or no (with reason)
- `AGENTS.md criteria`: the criterion the test passes (real-code-path / strong-assertion / behavior-not-implementation / non-duplicate)

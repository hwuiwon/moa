# Mutation Verification

The single most important step in test authoring. Without it, you have written code that compiles, not a test that catches regressions.

## What It Is

Mutation verification is the discipline of:

1. Writing the test until it passes against the correct implementation.
2. Mutating the implementation to introduce a plausible bug.
3. Confirming the test fails with a message that names the bug.
4. Reverting the mutation.
5. Re-running the test to confirm green again.

If step 3 fails — the test still passes against the broken implementation — the test is too weak. Strengthen it before merging.

## Why It Matters

Most weak tests pass because of accidental coupling, not because they verify the behavior. Examples of tests that pass against broken implementations:

- A test that asserts `result.is_err()` passes when the SUT now returns a panic instead of a `Result`.
- A test that asserts `events.len() >= 1` passes when the SUT now emits 17 events instead of 3.
- A test that asserts `status == Completed` passes when the SUT now skips three intermediate states.

Mutation-verifying catches all of these in one pass, before merge.

## How to Pick a Mutation

Choose a mutation that represents a real regression that could plausibly happen in this code. Generic patterns:

| Code shape | Plausible mutation |
|---|---|
| `if x > threshold` | Change `>` to `>=` or `<` |
| `match status { ... }` | Delete one arm; let it fall through |
| `for event in events { emit(event) }` | Replace with `for event in events.iter().take(1)` |
| State transition `Running -> WaitingForApproval` | Skip the transition entirely |
| Validation guard `if input.is_empty() { return Err(...) }` | Comment out the guard |
| `INSERT ... RETURNING id` | Drop the `RETURNING` and return a hardcoded ID |
| `tx.commit().await?` | Replace with `tx.rollback().await?` |
| Sequence number assignment | Reuse the previous number instead of incrementing |
| Workspace GUC `SET LOCAL` | Change to `SET` (leaks across pool checkouts) |
| Relational graph query with scoped storage predicates | Remove the `ScopedConn`/GUC setup or drop the `storage_partition_id`/`scope` filter |

Pick the one that would be hardest to spot in a code review. If the test catches that one, it is strong.

## Per-Tier Mutation Examples

### Unit tier

Test: `parse_bash_command("npm test --watch")` returns `BashCommand { exe: "npm", args: ["test", "--watch"] }`.

Mutations to try:
- Change the splitting logic to split on `=` instead of whitespace; test should fail because args are wrong.
- Drop the first token from args (off-by-one); test should fail.
- Return `Vec::new()` for args; test should fail.

### Integration tier

Test: a session lifecycle test that posts a message, waits for completion, and asserts the event sequence.

Mutations to try:
- Comment out the `Event::SessionCompleted` emit at the end of the run; test should fail because the final status is wrong.
- Reorder the event emit so `BrainResponse` happens before `ToolResult`; test should fail because the sequence is wrong.
- Replace `match` arms in the orchestrator with a single fallthrough; test should fail because some lifecycle edge gets skipped.

### Snapshot tier

Test: an `insta::assert_snapshot!` against the rendered Anthropic request body.

Mutations to try:
- Remove the `cache_control` marker from the system prompt; the snapshot diff should fail loudly.
- Change the model ID; the snapshot diff should fail.
- Reorder messages; the snapshot diff should fail.

If the snapshot test does not fail on these, the snapshot is too coarse — likely matching only on a top-level shape and missing the nested fields.

### Live tier

Test: a live Anthropic call that asserts on usage tokens.

Mutations to try (in the offline counterpart, which should mirror the live test's structure):
- Change the token-extraction path from `usage.input_tokens` to `usage.output_tokens`; test should fail.
- Hardcode the token value to `0`; test should fail.

For pure live tests, mutation-verify against the offline counterpart instead. Direct mutation against a live SUT is impractical.

### Eval scenario tier

Test: a long-conversation scenario with planted facts.

Mutations to try:
- Disable the memory-recall step; planted-fact assertions should fail.
- Disable the cache; the cache-hit assertion should fail.
- Increase the prompt size; the cost assertion should fail.

## When the Mutation Doesn't Trigger a Failure

If the test passes against a clearly-broken implementation, one of three things is true:

1. **The assertion is too weak.** Tighten it. Look for `>= 1`, `is_err()`, substring matches that should be structural matches.
2. **The mutation didn't reach the tested code path.** The test exercises a different code path than the one mutated. Either the test is testing the wrong thing, or the mutation needs to be elsewhere.
3. **The behavior is genuinely covered by another test.** Check if a different test in the suite caught the mutation. If yes, this test is redundant; consider deleting it instead of strengthening it.

## When to Skip Mutation-Verify

There are two legitimate cases:

1. **The test is a documentation example** that demonstrates a public API and is not intended to catch regressions.
2. **The test is a snapshot of an external contract** (provider request body, gateway message format) where the snapshot itself is the assertion and any change to the SUT will visibly diff the snapshot.

Both cases should be marked in the test's doc comment so the next reader does not strengthen them unnecessarily.

## Mutation-Verify in the Reporting Output

When reporting on a new test, include the mutation-verify line in the output format:

> Mutation verified: yes (commented out the `Event::SessionCompleted` emit at line 142 of `session_engine.rs`; test failed at the status sequence assertion as expected)

> Mutation verified: no (test is a snapshot of the external Anthropic request shape; changes to the SUT will visibly diff the snapshot)

Either is acceptable in review. "I forgot" is not.

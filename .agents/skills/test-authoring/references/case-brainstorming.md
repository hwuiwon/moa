# Case Brainstorming

What cases to enumerate before writing a test, by tier.

## Universal Minimum

For any test at any tier, cover at least:

1. **Happy path** — the most common production input.
2. **Most-likely error path** — what is the most common way this will fail in production?
3. **One boundary** — empty input, max size, zero, max integer, missing optional field.

If the SUT cannot fail, you do not need a test for "most-likely error path"; document why in the test's doc comment.

## Per-Tier Additions

### Unit

Add:

- **Adversarial input**: malformed strings, deeply nested data, Unicode edge cases, numerical overflow.
- **Identity properties**: `parse(format(x)) == x`, `decode(encode(x)) == x` where applicable.
- **Boundary algebra**: zero, one, two, max-1, max, max+1.

### Integration

Add the unit list, plus:

- **Idempotency**: running the same operation twice produces the same result. Critical for migrations, memory writes, and event emits.
- **Ordering**: when ordering matters (FIFO queues, sequence numbers, event log), assert exact order, not membership.
- **Concurrency**: when multiple actors can write to the same surface, assert the concurrent outcome matches the serial outcome.
- **Cleanup / teardown**: does dropping the SUT release resources? Does the next test run cleanly?

### Snapshot

Add:

- **Determinism check**: run the SUT twice in the same test and assert byte-equality before snapshotting.
- **Redaction completeness**: list every non-deterministic field (timestamps, UUIDs, request IDs) and confirm the snapshot redacts each one.
- **Cross-version stability**: if the SUT depends on a serializer that may reorder keys (HashMap, BTreeMap), prove the canonical ordering is enforced.

### Live

Add:

- **Authentication failure**: 401, 403 from the upstream service.
- **Rate limit**: 429 with retry-after.
- **Server error**: 500, 502, 503.
- **Malformed response**: upstream returns valid HTTP but invalid JSON.
- **Disconnection mid-stream**: for streaming endpoints, the connection drops between chunks.

The wiremock-based offline counterpart should cover all five of these. The live test itself usually covers only the happy path because reproducing live failure modes against a real service is brittle.

### Eval Scenario

Add:

- **Planted facts**: list 5-10 specific behaviors the agent must exhibit during the conversation; assert each one independently.
- **Budget regressions**: assert latency, cost, cache hit ratio against a baseline.
- **Negative behaviors**: assert the agent does NOT do specific things (call a forbidden tool, leak a canary, bypass an approval).
- **Recovery from injected errors**: place a tool error mid-scenario; assert recovery.

## Specific to MOA Surfaces

### Orchestrator lifecycle tests

Always include:

- blank session waits for the first message
- queued messages stay FIFO
- approval persists, pauses, resumes
- soft cancel stops cleanly without inventing extra turns

These are the core lifecycle assertions; the brain harness suite in `crates/moa-brain/tests/brain_turn_db.rs` and the Restate suite in `crates/moa-orchestrator/tests/session_turn_lifecycle_service_e2e.rs` exercise them.

### Provider tests

Always include:

- request body shape (snapshot)
- streaming response parsing
- tool call extraction
- usage token accounting
- cost-cents derivation against a versioned pricing fixture

### Memory tests

Always include:

- workspace isolation: a write in workspace A is not visible from workspace B
- supersession: a later write replaces an earlier one with a `SUPERSEDES` edge
- changelog DAG: no cycles in `cause_change_id`
- RLS enforcement: queries running under `BYPASSRLS` should not be the only ones that pass; the policy itself must be tested

### Approval tests

Always include:

- AllowOnce / AlwaysAllow / Deny — all three decisions exercised
- Rule persistence: AlwaysAllow survives session end and orchestrator restart
- Rule scoping: a rule for `npm test*` does not match `npm test && rm -rf /`

### Session-store tests

Always include:

- monotonicity of `sequence_num` per session under concurrent emits
- append-only enforcement (UPDATE/DELETE blocked at the DB layer)
- replay produces the same context on a fresh process

## What Not to Brainstorm

- **Cases that exist only because the language allows them.** Do not test "what if `option` is `None`" if `option` cannot be `None` at the call site.
- **Cases that duplicate the type system.** Do not test that a `u32` rejects negative numbers.
- **Cases that exist only to inflate coverage.** A test that exists to make a coverage tool happy is a test that fails AGENTS.md's first criterion.

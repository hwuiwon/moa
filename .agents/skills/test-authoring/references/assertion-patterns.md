# Assertion Patterns

Concrete patterns for strong assertions, and the anti-patterns that make tests pass tautologically or break on every refactor.

## The Strong-Assertion Rules

### 1. Exact counts, not "at least one"

A test that asserts `>= 1` matches a regression that produces 0, 1, 2, or 17 events. A test that asserts `== 3` catches the regression that produces 2 or 4.

```rust
// Weak
assert!(events.iter().any(|e| matches!(e, Event::ToolCall { .. })));

// Strong
let tool_calls: Vec<_> = events.iter()
    .filter(|e| matches!(e, Event::ToolCall { .. }))
    .collect();
assert_eq!(tool_calls.len(), 3, "expected exactly 3 tool calls, got {}", tool_calls.len());
```

If you do not know the exact count, the test is not pinning the behavior; it is hoping for the behavior.

### 2. Structured equality on values you control

When the value is something the SUT produces and the test controls the inputs, assert structural equality. Substring-matching against a free-text field couples the test to the rendering, not to the behavior.

```rust
// Weak
assert!(rendered.contains("approved"));

// Strong
assert_eq!(decision.kind, ApprovalDecisionKind::AllowOnce);
assert_eq!(decision.resource, "bash:npm test");
```

Substring matching is appropriate for free-form text the SUT did not generate (e.g., LLM output in a live test). It is not appropriate for IDs, statuses, counts, or any value the SUT produces deterministically.

### 3. Pin sequences, not endpoints

A lifecycle test that asserts only the final status misses every reordering bug.

```rust
// Weak
assert_eq!(session.status, SessionStatus::Completed);

// Strong
let statuses: Vec<_> = events.iter()
    .filter_map(|e| match e {
        Event::SessionStatus { status, .. } => Some(*status),
        _ => None,
    })
    .collect();
assert_eq!(statuses, vec![
    SessionStatus::Running,
    SessionStatus::WaitingForApproval,
    SessionStatus::Running,
    SessionStatus::Completed,
]);
```

The strong version catches: skipping `WaitingForApproval` entirely, never returning to `Running`, double-emitting `Completed`, all of which are real regressions the weak version misses.

### 4. `expect("specific message")` over `unwrap()`

In test code, `unwrap()` produces panics that say "called `unwrap()` on `None`" with no context. `expect("session should be created in setup")` tells the next reader what was supposed to happen.

```rust
// Weak
let session = harness.start_session(req).await.unwrap();

// Strong
let session = harness.start_session(req)
    .await
    .expect("setup: starting a fresh session should always succeed");
```

This is the one place in the codebase where `expect` is preferred over `?`-propagation. Tests are not error-handling code; they are assertions that should produce informative failures.

### 5. Assert the failure mode, not just the failure

When testing an error path, assert which error occurred, not just that some error occurred.

```rust
// Weak
assert!(result.is_err());

// Strong
let err = result.expect_err("malformed input should fail");
assert!(matches!(err, MyError::MalformedInput { field, .. } if field == "user_id"));
```

A test that asserts `is_err()` passes when the SUT panics, returns the wrong error type, or returns a less-informative error than it should.

## The Anti-Patterns

### Tautological tests

Tests that pass because the type system already guarantees the property:

```rust
// Useless
let x: u32 = 5;
assert!(x >= 0); // u32 cannot be negative

// Useless
let s = "hello".to_string();
assert_eq!(s.len(), s.len()); // self-equality
```

If a test cannot fail without changing the SUT, it is not testing the SUT.

### Mock-driven tests

Tests that mock so much of the SUT's environment that the test passes when only the mock works:

```rust
// Useless
let mock_provider = ScriptedProvider::new(vec![Response::Ok("done")]);
let result = mock_provider.complete(req).await.unwrap();
assert_eq!(result.text, "done"); // tests the mock, not anything in MOA
```

Mocks are tools for isolating the real SUT. If the test asserts only on values that came from the mock and nothing transformed by the SUT, the test is not testing the SUT.

### Implementation-coupled tests

Tests that break on refactor without a corresponding behavior change:

```rust
// Brittle: couples to internal field order
assert_eq!(format!("{:?}", session), "Session { id: ..., status: Running, ... }");

// Brittle: couples to internal call counts
assert_eq!(provider.call_count(), 3); // when the spec is "until the brain stops"

// Brittle: couples to private struct shape
assert!(session.internal_state.queue.is_empty());
```

Test the behavior the public API promises, not how the implementation happens to achieve it.

### Coverage-padding tests

Tests written only to make a coverage tool report a higher number:

```rust
#[test]
fn default_works() {
    let _ = MyType::default();
}

#[test]
fn debug_works() {
    let _ = format!("{:?}", MyType::default());
}
```

These tests pass because the language guarantees they pass. They never catch a regression. Delete on sight.

### "It compiled" tests

Tests that exist only to confirm a struct can be constructed:

```rust
#[test]
fn can_construct() {
    let _foo = Foo { a: 1, b: 2, c: 3 };
}
```

If `cargo build` would have caught the same regression, the test is not adding value over `cargo build`.

## When Substring Matching Is Acceptable

Substring matching against the SUT's output is acceptable in three cases:

1. **Live LLM output in a live test**: the LLM's free-text response cannot be asserted structurally.
2. **Error messages from external systems**: a Postgres error string is not part of MOA's contract; substring-matching the relevant token is fine.
3. **Logging assertions**: when asserting that a `tracing` span fired, substring-matching the span name in captured output is acceptable.

In all three, prefer asserting on a structured field if one is available (LLM tool calls have a structured shape; Postgres errors have an error code; tracing spans have a `name()` method).

## Specific Patterns for MOA

### Snapshot tests

Use `insta::assert_snapshot!` for byte-stable string outputs and `insta::assert_json_snapshot!` for JSON. Always:

- Set redactions for `id`, `created_at`, `updated_at`, `request_id`, and any UUID-shaped field.
- Run the SUT twice and assert byte-equality before snapshotting; if the SUT is non-deterministic, snapshots will churn.
- Name snapshots descriptively: `session_lifecycle__approval_resume.snap`, not `test_1.snap`.

### Postgres-backed integration tests

- Acquire a `ScopedConn` through the test bootstrap, not raw `sqlx::PgPool::acquire()`.
- Set the workspace GUC inside the same transaction as the workload.
- Roll back at the end of each test; do not assume cleanup between runs.

### Provider tests

- Wiremock-based offline tests should assert on the request body, not just that a request was made.
- Live provider tests should assert on usage tokens and structured response fields, not on the free-text response content.

### Eval scenarios

- Each scenario asserts independently on planted facts; one scenario's failure should not mask another's.
- Cost and latency assertions should reference a versioned baseline file, not hardcoded numbers in the scenario itself.

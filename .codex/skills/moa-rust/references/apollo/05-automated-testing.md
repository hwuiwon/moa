# Chapter 05 Notes: Automated Testing

## Shape Of Good Tests

- Tests should document behavior, not just catch regressions.
- Prefer descriptive names that read like the behavior under test.
- Keep one behavior per test where practical.
- Use modules to group tests around the function or behavior being exercised.
- Doc tests are useful for stable public API examples.

## MOA Translation

- Follow the repo rule: integration tests in crate `tests/` directories, unit tests inline under `#[cfg(test)]`.
- For public APIs with doc comments, examples are valuable when they are stable and cheap to maintain.
- Use `expect("specific failure message")` in tests when that makes failures clearer.
- For async code, keep tests focused on one behavior and isolate setup helpers instead of stacking assertions.

## Review Questions

- Does the test name explain the behavior and failure condition?
- Is this test checking one thing, or several unrelated things?
- Would an integration test better cover the real boundary here?
- If the API is public, should this be a doc example as well?

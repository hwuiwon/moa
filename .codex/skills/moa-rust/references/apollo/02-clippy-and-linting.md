# Chapter 02 Notes: Clippy And Linting

## Baseline

- Treat Clippy as part of the normal edit loop.
- Apollo's default command matches MOA's close-out posture closely:
  `cargo clippy --all-targets --all-features --locked -- -D warnings`

## Lints Worth Watching

- `redundant_clone`
- `needless_borrow`
- `manual_ok_or`
- `large_enum_variant`
- `clone_on_copy`
- `needless_collect`
- `unnecessary_wraps`

## Suppression Policy

- Fix the code rather than silence the lint.
- If suppression is necessary, prefer `#[expect(clippy::lint_name)]` plus a reason.
- Avoid broad or crate-wide `#[allow(...)]` without a documented justification.

## MOA Translation

- Because MOA requires `cargo clippy ... -D warnings` before code work is considered complete, lint cleanliness is a repo rule, not a style preference.
- Lint fixes should not fight the documented architecture. If Clippy suggests a simplification that obscures a trait boundary or state model, keep the architecture explicit and justify the exception.

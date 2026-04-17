# Chapter 04 Notes: Errors And Results

## Default Approach

- Prefer `Result<T, E>` over panic for fallible work.
- Avoid `unwrap()` and `expect()` in production paths.
- Use `thiserror` for crate-level errors.
- Reserve `anyhow` for binary entrypoints and lightweight test helpers.
- Prefer `?` for propagation; use `map_err`, `or_else`, `if let`, or `inspect_err` when recovery or inspection is needed.

## MOA Translation

- MOA already standardizes this split: library crates use `thiserror`, while `anyhow` is allowed only in `moa-cli` and `moa-desktop`.
- When adding crate errors, prefer one crate-local error enum with explicit variants over stringly typed propagation.
- If a function cannot actually fail, do not return `Result` just because nearby code does.
- If a failure is expected and normal, `let-else` or a small guard often reads better than a large `match`.

## Review Questions

- Is this a library path that should expose a typed error instead of `anyhow`?
- Did a new panic slip into non-test code?
- Can a stringly `StorageError` or `ProviderError` variant be made more precise inside the crate?
- Are failure paths tested?

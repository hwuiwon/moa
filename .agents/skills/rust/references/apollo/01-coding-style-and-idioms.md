# Chapter 01 Notes: Coding Style And Idioms

## What To Carry Into MOA

- Prefer borrowing over cloning. If a function always clones a borrowed input, the signature is probably wrong.
- Prefer `&str` over `String` and slices over `Vec<T>` in read-only APIs.
- Pass small `Copy` types by value when that makes call sites simpler.
- Use `let-else`, `if let`, and `?` to keep `Option` and `Result` handling flat and readable.

## MOA Translation

- Trait methods in `docs/01-architecture-overview.md` should keep ownership decisions intentional. Match the documented API shape before "fixing" clones locally.
- If a type is shared across tasks, provider calls, or orchestrator state, prefer references or `Arc` over ad hoc cloning.
- Avoid early allocation. If a pipeline step can stream, iterate, or borrow, do that before collecting.

## Watch For

- `.clone()` inside loops, iterator chains, or event processing paths
- `String` parameters where `&str` would work
- `Vec<T>` parameters where `&[T]` would work
- manual `match` blocks that become clearer as `?`, `ok_or_else`, or `let-else`

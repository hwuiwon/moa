# Apollo Chapter Map

These files keep the Apollo handbook material local to this skill, in the same style as skills that ship nested Markdown references.

Use only what the task needs:

- [01-coding-style-and-idioms.md](01-coding-style-and-idioms.md): default pick for ownership, signatures, `Option`, and `Result` flow
- [02-clippy-and-linting.md](02-clippy-and-linting.md): use when lint feedback or lint policy matters
- [03-performance-mindset.md](03-performance-mindset.md): use when the task mentions speed, memory, clones, or hot paths
- [04-errors-and-results.md](04-errors-and-results.md): use for error enums, propagation, panic removal, and binary-versus-library error choices
- [05-automated-testing.md](05-automated-testing.md): use when adding or reviewing tests
- [06-generics-and-dispatch.md](06-generics-and-dispatch.md): use for trait design, object safety, and `dyn Trait` decisions
- [07-type-state-pattern.md](07-type-state-pattern.md): use for compile-time state modeling or builder APIs with required transitions
- [08-comments-vs-documentation.md](08-comments-vs-documentation.md): use for doc comment and inline comment decisions
- [09-understanding-pointers.md](09-understanding-pointers.md): use for `Arc`, `Mutex`, `RwLock`, `Send`, `Sync`, and interior mutability

MOA-specific repo rules still win. Read [../moa-rust-rules.md](../moa-rust-rules.md) before treating any generic Rust advice as binding.

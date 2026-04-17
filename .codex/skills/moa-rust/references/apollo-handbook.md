# Apollo Rust Handbook Index

These are local, MOA-oriented notes derived from Apollo GraphQL's Rust Best Practices Handbook. Use this file as the table of contents, then open only the chapters relevant to the task.

## Local Chapter Notes

- [references/apollo/README.md](apollo/README.md): chapter map and selection guide
- [references/apollo/01-coding-style-and-idioms.md](apollo/01-coding-style-and-idioms.md): borrowing, ownership, `Copy`, `Option` and `Result` flow
- [references/apollo/02-clippy-and-linting.md](apollo/02-clippy-and-linting.md): lint loop, key Clippy lints, `#[expect]`
- [references/apollo/03-performance-mindset.md](apollo/03-performance-mindset.md): measure-first optimization, clones, stack versus heap, iterators
- [references/apollo/04-errors-and-results.md](apollo/04-errors-and-results.md): `Result`, `thiserror`, `anyhow`, propagation
- [references/apollo/05-automated-testing.md](apollo/05-automated-testing.md): test naming, test shape, doc tests
- [references/apollo/06-generics-and-dispatch.md](apollo/06-generics-and-dispatch.md): static versus dynamic dispatch
- [references/apollo/07-type-state-pattern.md](apollo/07-type-state-pattern.md): compile-time state modeling
- [references/apollo/08-comments-vs-documentation.md](apollo/08-comments-vs-documentation.md): `//`, `///`, `//!`, docs hygiene
- [references/apollo/09-understanding-pointers.md](apollo/09-understanding-pointers.md): `Arc`, `Mutex`, `RwLock`, `Send`, `Sync`

## How To Use This Index

- Start with `01`, `02`, and `04` for most MOA code changes.
- Add `03` when the work is performance-sensitive or clone-heavy.
- Add `05` and `08` when shaping tests or documentation.
- Add `06`, `07`, and `09` when designing traits, async shared state, or stateful APIs.

The upstream source is `apollographql/rust-best-practices`, but the skill should load these local notes instead of jumping out to remote chapter Markdown.

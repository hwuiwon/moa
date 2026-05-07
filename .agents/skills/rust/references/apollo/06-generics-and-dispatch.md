# Chapter 06 Notes: Generics And Dispatch

## Decision Rule

- Static where you can, dynamic where you must.

## Practical Guidance

- Prefer generics and static dispatch when the concrete type is known and performance or clarity matters.
- Use `dyn Trait` when runtime heterogeneity, plugin boundaries, or stable interface hiding are the real requirements.
- Prefer `&dyn Trait` over `Box<dyn Trait>` when ownership is not needed.
- Box at API boundaries rather than deep inside internal structs unless recursion or indirection is actually required.

## MOA Translation

- MOA's documented traits are stable boundaries between crates and runtime implementations. Dynamic dispatch is often appropriate at those crate boundaries.
- Inside a crate, avoid boxing too early if a generic helper or concrete type keeps the code simpler.
- When threading through orchestrators, registries, or providers, confirm whether the abstraction is architectural or just incidental convenience.

## Review Questions

- Is `dyn Trait` needed because multiple implementations coexist at runtime?
- Did boxing happen too early inside internal state?
- Would a generic helper reduce allocation or simplify error messages?
- Is the trait object actually object-safe?

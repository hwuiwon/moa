# Chapter 08 Notes: Comments Versus Documentation

## Split The Roles

- `//` comments explain why, safety, workarounds, or design context.
- `///` documents public items.
- `//!` documents module or crate purpose.

## Anti-Patterns

- Comments that just narrate obvious code
- stale "living documentation"
- anonymous `TODO`s with no issue or owner
- long comment blocks that should be extracted into code or moved into design docs

## MOA Translation

- MOA explicitly requires module-level doc comments and public function doc comments.
- Design-context comments are useful when they point back to `docs/` decisions or invariants that are not obvious from code alone.
- Inline comments should be rare and should explain the why, not the mechanics.

## Review Questions

- Does this module start with `//!`?
- Do all public functions have meaningful docs?
- Is this inline comment actually useful, or should the code be renamed or extracted?
- Should this TODO reference an issue?

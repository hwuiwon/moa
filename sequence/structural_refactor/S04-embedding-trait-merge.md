# S04 — Consolidate `EmbeddingProvider` and `Embedder` into one trait in `moa-core`

## Scope

The workspace currently has two parallel embedding abstractions:
- `moa_providers::EmbeddingProvider` (in `moa-providers/src/embedding.rs`)
- `moa_memory_vector::Embedder` (in `crates/moa-memory/vector/src/`)

They model the same concept (`embed(&[String]) -> Vec<Vec<f32>>`). Merge them into a single trait in `moa-core::traits`. Update both crates and all consumers to use the unified trait. **No behavior changes; the resulting trait is the union of both current contracts.**

## Preconditions

- S01–S03 complete and merged.
- `cargo check --workspace` is green.

## Why this prompt

This is the most consequential cross-crate coupling fix in the whole pack. As long as two trait definitions exist for the same concept, every consumer (`moa-brain`, `moa-eval`, `moa-memory-ingest`, etc.) has to pick a side and write boilerplate to bridge. Consolidating into `moa-core` lets every consumer depend on one trait, lets every adapter (Cohere, Gemini, OpenAI) implement one trait, and removes a class of "where does this go?" decisions from the rest of the refactor.

## Files in scope

- `crates/moa-core/src/traits/embedding.rs` — new file containing the canonical trait
- `crates/moa-core/src/traits/mod.rs` — add `pub mod embedding; pub use embedding::*;`
- `crates/moa-providers/src/embedding.rs` — gut the trait definition; convert to be a re-export of the core trait + adapter impls only
- `crates/moa-memory/vector/src/lib.rs` (or wherever `Embedder` is defined) — same: gut the trait, keep impls
- Every call site that imported either trait — update imports to `moa_core::traits::EmbeddingProvider`

## Files explicitly out of scope

- The actual embedding *implementations* (Cohere, Gemini, OpenAI, Turbopuffer's caller). Their bodies don't change; only the trait they `impl` for.
- Any test that exercises embeddings. Those move with the trait but don't get rewritten.
- Vector storage logic in `moa-memory-vector` (Turbopuffer client, indexing, etc.). Out of scope.

## Step-by-step instructions

1. **Read both current trait definitions** carefully. Build a side-by-side comparison:
   ```
   moa_providers::EmbeddingProvider
     fn ...
     fn ...
   
   moa_memory_vector::Embedder
     fn ...
     fn ...
   ```
   Differences fall into three buckets:
   - **Identical with renamed methods** — pick the better name, keep behavior
   - **One is a strict superset of the other** — use the superset, give the smaller-trait impls default impls for the extra methods
   - **Genuinely divergent contracts** — STOP. This is not a clean merge. Document the divergence in `REFACTOR_NOTES.md` and ask the user before proceeding.

2. **Design the unified trait.** Likely shape:
   ```rust
   // crates/moa-core/src/traits/embedding.rs
   use async_trait::async_trait;
   use crate::error::Error; // or whatever the canonical error is
   
   /// Capability metadata for an embedding provider.
   #[derive(Debug, Clone)]
   pub struct EmbeddingCapabilities {
       pub model_id: String,
       pub dimensions: usize,
       pub max_batch_size: usize,
       pub max_input_tokens: usize,
       // ... whatever both current traits expose
   }
   
   #[async_trait]
   pub trait EmbeddingProvider: Send + Sync {
       fn name(&self) -> &str;
       fn capabilities(&self) -> EmbeddingCapabilities;
       async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, Error>;
       
       // Default impl for single-input convenience, if both traits had it
       async fn embed_one(&self, input: &str) -> Result<Vec<f32>, Error> {
           let mut v = self.embed(&[input.to_string()]).await?;
           v.pop().ok_or_else(|| /* ... */)
       }
   }
   ```
   The exact method names and signatures must be a faithful merge of the two current traits.

3. **Add `mod embedding;` and `pub use embedding::*;`** to `crates/moa-core/src/traits/mod.rs`.

4. **In `moa-providers`**, remove the local trait definition. Replace with:
   ```rust
   // crates/moa-providers/src/embedding.rs
   //! Embedding provider implementations.
   //! 
   //! The trait itself lives in `moa_core::traits::EmbeddingProvider`.
   //! This module contains adapter impls.
   
   pub use moa_core::traits::EmbeddingProvider;
   pub use moa_core::traits::EmbeddingCapabilities;
   
   // Keep the actual impl modules:
   pub mod cohere;
   pub mod gemini;
   pub mod openai;
   ```
   The `impl EmbeddingProvider for CohereEmbedder { ... }` blocks stay in their current files; only the trait definition is gone.

5. **In `moa-memory-vector`**, remove the local `Embedder` trait. Replace with:
   ```rust
   // crates/moa-memory/vector/src/lib.rs (or wherever it was)
   pub use moa_core::traits::EmbeddingProvider as Embedder;  // alias for back-compat
   // ... or just import EmbeddingProvider directly throughout.
   ```
   **Pick one**: either keep `Embedder` as a type alias temporarily for back-compat (cleaner diff in this prompt), or rename every call site (cleaner end state). Recommendation: alias here, sweep in S14.

6. **Update every consumer's imports.** Likely sites: `moa-brain`, `moa-eval`, `moa-memory-ingest`, `moa-orchestrator`. Search for:
   ```bash
   rg "use moa_providers::.*Embedding" crates/
   rg "use moa_memory_vector::.*Embedder" crates/
   ```
   Replace each with `use moa_core::traits::EmbeddingProvider;`.

7. **`Cargo.toml` updates.** Crates that previously depended on `moa-providers` *only* for the embedding trait can drop that dep — but only if they truly don't use anything else from `moa-providers`. Check carefully; do not remove a dep that's still needed for the LLM trait or factory.

8. **Add `moa-core` as a dep** to any crate that didn't already have it (most should already; `moa-core` is the foundation).

9. **Run verification.** Fix imports in any crate the compiler complains about.

10. **Document any divergence** between the original two traits that required a judgment call (e.g. method renamed, parameter type widened) in `REFACTOR_NOTES.md` under `[S04]`.

## Verification

```bash
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-run
# Specifically check the embedding-using crates
cargo test -p moa-providers --no-run
cargo test -p moa-memory-vector --no-run
cargo test -p moa-brain --no-run
cargo test -p moa-eval --no-run
```

If there are existing tests that exercise embedding adapters with feature gates or live API hits, do not run them here — just confirm they compile.

## Acceptance criteria

- [ ] `crates/moa-core/src/traits/embedding.rs` exists with the unified trait.
- [ ] `moa-providers` no longer defines an embedding trait; only impls.
- [ ] `moa-memory-vector` no longer defines an embedding trait; uses `moa_core::traits::EmbeddingProvider` (possibly via alias).
- [ ] Every `impl EmbeddingProvider for ...` and `impl Embedder for ...` block compiles against the new trait.
- [ ] All call sites import from `moa_core::traits`.
- [ ] `cargo check --workspace --all-targets` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [ ] No new dependencies were added beyond `moa-core` (which most crates already had).

## Rollback plan

`git revert` the prompt's commits. Because the trait moves cross multiple crates, partial rollback is messy — full revert is cleaner. Save the divergence notes from step 1 if rolling back, since they'll be useful on retry.

## Notes for the agent

- **The two current traits may have different error types.** That's the most likely real divergence. The unified trait should use whatever error type `moa-core` already exposes (almost certainly `moa_core::Error` or similar). If neither current trait uses the core error type, that's a smell — flag it in `REFACTOR_NOTES.md` and convert.
- **`async_trait` is the convention.** Don't try to make this an `impl Future` trait — that's a different design discussion.
- **Don't add capabilities the existing traits didn't have.** No "while we're here" — strict minimum, faithful merge.
- **Don't remove the `Embedder` alias in `moa-memory-vector`** in this prompt. Keeping it preserves call-site stability; S14 will sweep it.
- **If both traits already had `EmbeddingCapabilities`-style metadata structs**, merge those structs the same way — pick the union of fields.
- **Watch for `impl<T: EmbeddingProvider> ...` generic code** elsewhere. It must continue to compile because the trait surface didn't shrink. If it doesn't, the merge missed a method.
- **Time budget**: ~1 session. If divergence step (1) reveals a non-merge, STOP and ask the user.

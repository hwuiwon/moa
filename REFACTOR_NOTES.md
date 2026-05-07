# Refactor Notes

## [S01] Workspace Dependency Hoist

Inventory covered direct registry dependencies used by two or more workspace
crates across `[dependencies]`, `[dev-dependencies]`, and `[build-dependencies]`.

| Dependency | Versions found | Feature sets found | Action |
|---|---|---|---|
| `arrow` | `53` | none, `prettyprint` | Hoisted; `moa-lineage-cold` keeps `prettyprint`. |
| `axum` | `0.8` | none | Hoisted. |
| `base64` | `0.22` | none | Hoisted. |
| `blake3` | `1` | none | Hoisted. |
| `ed25519-dalek` | `2` | none, `pem` + `pkcs8` | Hoisted; `moa-lineage-audit` keeps signing-key features. |
| `globset` | `0.4` | none | Hoisted. |
| `hex` | `0.4` | none | Hoisted. |
| `moka` | `0.12` | `future` | Hoisted with `future`. |
| `object_store` | `0.11` | `aws` | Hoisted with `aws`. |
| `parquet` | `53` | `async` + `arrow` | Hoisted with `async` + `arrow`. |
| `regex` | `1` | none | Hoisted. |
| `secrecy` | `0.10` | none | Hoisted. |
| `serde_canonical_json` | `1` | none | Hoisted. |
| `sha2` | `0.10` | none | Hoisted. |
| `shell-words` | `1.1` | none | Hoisted. |
| `sqlx` | `0.8` | `runtime-tokio`/`runtime-tokio-rustls`, `tls-rustls`, `postgres`, `chrono`, `uuid`, `json`, `macros`, `migrate` | Hoisted with base `runtime-tokio` + `postgres`; crates keep only extra features. |
| `tempfile` | `3` | none | Hoisted. |
| `toml` | `0.8` | none | Already hoisted; remaining literal uses converted. |

No repeated dependency from the S01 inventory was left unhoisted for feature
divergence or version mismatch. Dependencies that were already inherited from
`[workspace.dependencies]` before S01 were left as-is unless they still had
literal per-crate version specs.

## [S02] cargo-hakari Workspace Hack

`cargo-hakari` generated `crates/workspace-hack/` and added a
`workspace-hack` dependency to every workspace crate. The active hakari config
for `cargo-hakari 0.9.37` lives in `.config/hakari.toml`; the root
`[workspace.metadata.hakari]` section mirrors the requested package, resolver,
and platform settings for discoverability.

Maintenance requirement: after any dependency or feature change, run
`cargo hakari generate && cargo hakari manage-deps --yes && cargo hakari verify`
and commit the resulting manifest updates. CI now runs `cargo hakari verify` in
the deploy test job before formatting, clippy, and tests.

## [S04] Embedding Trait Consolidation

`moa_providers::EmbeddingProvider` and `moa_memory_vector::Embedder` were
mergeable but used different method names and error types. The canonical trait
now lives at `moa_core::traits::EmbeddingProvider` with the union of the old
method surface: `model_id`/`dimensions`, `model_name`/`model_version`/
`dimension`, and `embed`.

`moa-memory-vector` keeps `Embedder` as a compatibility alias to the core trait.
Vector embedder implementations now return `moa_core::MoaError` from the trait
method; the crate maps its local embedder errors into core provider/http/storage
errors at that boundary. Vector storage APIs and implementation bodies were not
otherwise changed.

## [S05] Orchestrator SessionStore Trait Audit

Verdict: LEAVE. `moa-orchestrator` defines `RestateSessionStore`, a Restate RPC
facade externally named `SessionStore`, not a duplicate Rust trait of
`moa_core::traits::SessionStore`. See `struct-pack/S05-decision.md`.

## [S07] moa-session Store and Query Split

`moa-session` now uses folder modules for `store/` and `queries/`. The split was
mechanical: SQL strings and method signatures were moved without behavior
changes, and the single `impl SessionStore for PostgresSessionStore` remains in
`store/session_store.rs`.

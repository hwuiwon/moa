# moa-dlp

Request-scoped, provenance-aware DLP tokenization for provider egress.
Replaces detected PII spans (`moa_memory_pii::PiiSpan`) with per-request random
tokens before text leaves the trust boundary, then restores them based on where
each value came from and where the output is going.

## Structure

- `lib.rs` — `tokenize` / `detokenize` convenience entry points over a fresh
  request-scoped vault.
- `vault.rs` — `TokenVault` plus the provenance model: `TokenSource`,
  `TokenSourceRole`, `TokenVisibility`, and `TokenDestination` decide which
  tokens may be restored for a given destination.
- `error.rs` — `Error` / `Result`, including atomic span validation failures.

## Notes

Token namespaces are random per request, so identical values in different
requests never mint a stable correlation handle. Span validation is atomic: an
invalid classifier offset never partially mutates a vault.

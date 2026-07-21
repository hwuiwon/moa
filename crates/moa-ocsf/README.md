# moa-ocsf

OCSF v1.3 security-event emission, signing, and persistence for MOA's audit
trail. Events are canonicalized, HMAC-signed per tenant, and written to the
`security_events` table.

## Structure

- `classes.rs` — OCSF v1.3 event class shapes emitted by MOA.
- `enums.rs` — OCSF v1.3 enum values used by those events.
- `emit.rs` — high-level emission helpers: `emit_*` (and transaction-scoped
  `emit_*_tx`) plus non-blocking `spawn_*` variants for authentication,
  authorization, API keys, approvals, delegation, SCIM, and data access.
- `audit_sink.rs` — `init_background_audit`, the batched non-fatal background
  writer behind the `spawn_*` helpers.
- `signing.rs` — per-tenant HMAC-SHA256 signing, key rotation, and
  verification.
- `jcs.rs` — RFC 8785 JSON Canonicalization Scheme; signatures cover the
  canonical bytes, not a transport encoding.

## Emission modes

`emit_*` helpers are synchronous and fail closed: a signing or insert failure
returns an error so the caller can roll back the action that would otherwise
lack an audit record. `spawn_*` helpers hand the event to the background batch
writer and never block or fail the caller; they are used on hot request paths
(authentication, authorization denials).

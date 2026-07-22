# OCSF Security Audit

MOA emits OCSF v1.3 security events for authentication, authorization,
API-key lifecycle, agent lifecycle, approval decisions, and SCIM
provisioning/deactivation. A bounded background sink signs events with a
per-tenant HMAC key and writes them to Postgres without blocking request
handling.

## Event Volume

Authentication and denied authorization events are emitted by default.
Allowed authorization decisions are high volume and are controlled by:

```sh
MOA_AUDIT_SECURITY_EMIT_AUTHZ_ALLOWS=true
```

For most tenants, keep allow-emission off unless a compliance posture requires
full allow/deny audit trails.

## Signing Keys

Each tenant has one active HMAC-SHA256 signing key. Create or rotate keys with:

```sh
curl -X POST http://localhost:10010/Tenants/ensure_signing_key \
  -H "Content-Type: application/json" \
  --data '"<tenant_uuid>"'
curl -X POST http://localhost:10010/Tenants/rotate_signing_key \
  -H "Content-Type: application/json" \
  --data '"<tenant_uuid>"'
```

Old key rows stay in `tenant_signing_keys` so historical events remain
verifiable after rotation.

## Verify An Event

```sh
curl -X POST http://localhost:10000/v1/audit/verify \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer <edge-token>" \
  --data '{"event_id":"<event_uuid>","tenant_id":"<tenant_uuid>"}'
```

`valid: true` means the stored canonical JSON bytes still match
`signature_hex` for the event's `signing_key_id`. `valid: false` means the row
was tampered with or the wrong signing key was used. This is a direct
`moa-edge` read route with tenant-admin authorization, not a Restate read
service.

## Persistence And Failure Behavior

The in-process sink batches inserts into `security_events`. A full queue,
signing failure, or insert failure drops the affected event rather than failing
the caller and increments `moa_ocsf_audit_events_dropped_total`. Alert on any
non-zero increase because it means the Postgres audit trail is incomplete.

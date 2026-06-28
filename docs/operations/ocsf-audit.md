# OCSF Security Audit

MOA emits OCSF v1.3 security events for authentication, authorization,
API-key lifecycle, agent lifecycle, approval decisions, and SCIM
provisioning/deactivation. Events are written synchronously to Postgres,
signed with a per-tenant HMAC key, and shipped to tenant-controlled S3 buckets
with Object Lock.

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

## Audit Destinations

Configure the per-tenant S3 destination:

```sh
curl -X POST http://localhost:10010/Tenants/set_audit_destination \
  -H "Content-Type: application/json" \
  --data '{"tenant_id":"<tenant_uuid>","bucket":"<customer_bucket>","region":"us-east-1","assume_role":"<role_arn>","retention_days":2190}'
```

The shipper reads `tenant_audit_destinations`, groups unshipped
`security_events` by tenant, writes gzipped NDJSON under the configured prefix,
and requests Object Lock COMPLIANCE retention on each object.

## Verify An Event

```sh
curl -X POST http://localhost:8080/v1/audit/verify \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer <edge-token>" \
  --data '{"event_id":"<event_uuid>","tenant_id":"<tenant_uuid>"}'
```

`valid: true` means the stored canonical JSON bytes still match
`signature_hex` for the event's `signing_key_id`. `valid: false` means the row
was tampered with or the wrong signing key was used. This is a direct
`moa-edge` read route with tenant-admin authorization, not a Restate read
service.

## Shipper Configuration

`services/audit-shipper` keeps the existing PostgreSQL log shipping path and
adds a `security_events` source when `MOA_DATABASE_URL` is set. Existing pgaudit
shipping still uses `BUCKET`; OCSF events use the per-tenant destination table.

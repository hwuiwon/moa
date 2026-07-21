# Subject access export runbook

MOA supports GDPR Article 15 subject access exports with the hosted privacy API:

```sh
curl -sS "$MOA_EDGE_URL/v1/privacy/export" \
  -H "Authorization: Bearer $MOA_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "subject_user_id": "<subject-user-uuid>",
    "reason": "GDPR Art.15 request <ticket>",
    "approval_token": "<signed-platform-admin-jwt>",
    "pgp_recipient": null
  }'
```

The API exports rows attributable to the subject user inside the authenticated
tenant.

For agent-facing contacts, set `subject_user_id` to either the contact UUID or
`contact:<contact-uuid>`. Contact exports are resolved inside the authenticated
tenant. Verified contact exports include linked unverified contact memory by
default, and every exported JSONL row includes `privacy_subject_user_id` and
`privacy_subject_provenance` so linked rows are visible in the archive.

## Authorization

The request requires an Ed25519-signed approval JWT with:

- `sub`: approver identifier
- `jti`: unique token id
- `exp`: expiration timestamp
- `op`: `export`
- `subject_user_id`: the exported user UUID, contact UUID, or `contact:<uuid>`
- `tenant_id`: the request tenant UUID
- `role` or `roles`: includes `platform_admin`

The hosted privacy export API verifies the token with `MOA_PRIVACY_APPROVAL_PUBLIC_KEY_HEX` and blocks
JTI replay through `moa.audit_jti_used`.

## Manifest signing

Set the export signing key before calling the API:

```sh
export MOA_PRIVACY_EXPORT_SIGNING_KEY_HEX=<ops-ed25519-signing-key-hex>
export MOA_PRIVACY_EXPORT_SIGNING_KEY_ID=<kms-key-id-or-ops-key-label>
```

The archive includes `export/manifest.json` and `export/manifest.sig`.
`manifest.sig` is the raw Ed25519 signature over the exact bytes of
`manifest.json`. The manifest records the export public key and declares
`"encryption": "none"` because ADR 0001 defers envelope encryption and MOA stores
redacted graph-memory text at ingestion time. The manifest also records the
resolved subject list and provenance for contact exports.

## Contents

The tarball contains:

- `facts.jsonl`
- `entities.jsonl`
- `relationships.jsonl`
- `embeddings.jsonl`
- `skills.jsonl`
- `changelog.jsonl`
- `README.md`
- `manifest.json`
- `manifest.sig`

Vectors are exported for provenance. Treat the archive as PHI-adjacent even
though the memory text is already redacted.

## Audit trail

Each successful export writes `op='export'` to `moa.graph_changelog` with the
reason, subject user id, resolved subject list, artifact counts, approver id,
and approval token JTI.
M22 pgaudit captures the underlying reads and changelog insert in the Postgres
audit log stream.

## Optional PGP wrapping

Set `"pgp_recipient"` to an armored recipient key to include an additional PGP
wrapped archive. Deliver only through an approved secure channel.

## Operational checks

1. Confirm the approval ticket and subject identity.
2. Generate a short-lived approval JWT with matching `tenant_id` and record its
   JTI in the ticket.
3. Call `POST /v1/privacy/export` from an admin workstation with the ops signing
   key available to the service.
4. Verify the manifest signature before delivery.
5. Confirm a matching `op='export'` changelog row exists.
6. Store delivery evidence in the ticket; do not paste archive contents into
   chat or issue trackers.

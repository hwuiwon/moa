# Subject erasure runbook

MOA supports GDPR Article 17 erasure with a hosted hard-purge API:

```sh
curl -sS "$MOA_EDGE_URL/v1/privacy/erase" \
  -H "Authorization: Bearer $MOA_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "subject_user_id": "<subject-user-uuid>",
    "reason": "GDPR Art.17 request <ticket>",
    "dry_run": false,
    "contact_erasure_scope": null,
    "approval_token": "<signed-platform-admin-jwt>"
  }'
```

Use `"dry_run": true` first to list the candidate count and a sample of node ids
without writing graph, vector, approval-JTI, or changelog rows.

For agent-facing contacts, set `subject_user_id` to either the contact UUID or
`contact:<contact-uuid>`. Contact erasure always requires an explicit
`contact_erasure_scope`:

- `specified_contact`: erase only the requested contact subject.
- `specified_and_linked_contacts`: erase the requested verified contact plus
  linked unverified contact subjects in the same tenant.

Do not set `contact_erasure_scope` for normal MOA operator/admin users.

## Authorization

The request requires an Ed25519-signed approval JWT with:

- `sub`: approver identifier
- `jti`: unique token id
- `exp`: expiration timestamp
- `op`: `erase`
- `subject_user_id`: the erased user UUID, contact UUID, or `contact:<uuid>`
- `tenant_id`: the request tenant UUID
- `role` or `roles`: includes `platform_admin`

The hosted privacy erase API verifies the token with `MOA_PRIVACY_APPROVAL_PUBLIC_KEY_HEX`. For
non-dry-run erasures with matching candidates, it records the JTI in
`moa.audit_jti_used` so the approval cannot be replayed.

If the tenant opts into dual control by setting
`ComplianceConfig.require_dual_control_for_erasure` (default `false`), the
erase call additionally enforces a four-eyes gate: it consumes an approved
`moa.dual_control_request` row, owned by the dual-control schema, bound to this exact
erasure request, approved by a distinct second admin. Without that approval,
the erasure fails closed with 403.

## What gets erased

For every active `moa.node_index` row in the authenticated tenant whose
`user_id` or `properties_summary.user_id` matches the subject, MOA calls the
graph hard-purge path. That path deletes:

- the matching `moa.node_index` graph node row
- attached relational edge rows in `moa.edge_index`
- associated vector records, including `moa.embeddings` rows for pgvector-backed tenants

The operation does not decrypt data and has no crypto-shred mode. ADR 0001
deferred envelope encryption; erasure is hard-purge only.

If `MOA_PII_VAULT_SECRET_HEX` is configured, the erase call also erases matching
PII-vault subject pseudonyms for the tenant.

The typed learning closure also removes attributable experience records,
experience attributions, learning candidates, learning-log sources, and suite
contributions. Attributable artifact revisions are archived and cleared in
place so pinned revision identities remain valid while no definition, source,
package file, or serving state survives.

Contact sessions store memory under `contact:<contact-uuid>`. A bare contact
UUID in the erasure request resolves to that stored subject id before candidate
enumeration. Linked contact deletion is never implicit; it only happens when
`contact_erasure_scope` is `specified_and_linked_contacts`.

## Audit trail

Each purged node leaves a redacted `op='erase'` changelog row in
`moa.graph_changelog` with a redaction marker and an audit metadata object
containing the reason, approver id, approval token JTI, and subject user id.
After at least one node is erased, the API writes one summary `op='erase'` row
targeting the subject user.

Re-running after all matching nodes are gone returns `erased_count: 0` and writes
no new changelog rows.

## Operational checks

1. Confirm the erasure ticket, subject identity, and tenant.
2. Call `POST /v1/privacy/erase` with `"dry_run": true` and attach the
   candidate count to the ticket.
3. For contacts, record whether the approval covers only the specified contact
   or specified plus linked contacts.
4. Generate a short-lived approval JWT with `op='erase'` and matching
   `tenant_id`.
5. If `require_dual_control_for_erasure` is enabled for the tenant, ensure a
   second, distinct admin has approved the matching dual-control request
   before the non-dry-run call.
6. Call `POST /v1/privacy/erase` with `"dry_run": false`.
7. Confirm `erased_count` matches the approved candidate count.
8. Confirm a summary `op='erase'` changelog row exists for the subject.
9. Confirm a second run with a fresh approval token returns `erased_count: 0`.

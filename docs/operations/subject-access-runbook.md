# Subject access export runbook

MOA supports GDPR Article 15 subject access exports with:

```sh
moa privacy export \
  --user <subject-user-uuid> \
  --reason "GDPR Art.15 request <ticket>" \
  --approval-token <signed-platform-admin-jwt> \
  --out /secure/path/subject-export.tgz
```

Add `--workspace <workspace-id>` to restrict the export to one workspace.
Without `--workspace`, the command exports every workspace row attributable to
the subject user.

## Authorization

The command requires an Ed25519-signed approval JWT with:

- `sub`: approver identifier
- `jti`: unique token id
- `exp`: expiration timestamp
- `op`: `export`
- `subject_user_id`: the exported user UUID
- `role` or `roles`: includes `platform_admin`
- optional `workspace_id`: limits the token to one workspace

The CLI verifies the token with `MOA_PRIVACY_APPROVAL_PUBLIC_KEY_HEX` and blocks
JTI replay through `moa.audit_jti_used`.

## Manifest signing

Set the export signing key before running the command:

```sh
export MOA_PRIVACY_EXPORT_SIGNING_KEY_HEX=<ops-ed25519-signing-key-hex>
export MOA_PRIVACY_EXPORT_SIGNING_KEY_ID=<kms-key-id-or-ops-key-label>
```

The archive includes `export/manifest.json` and `export/manifest.sig`.
`manifest.sig` is the raw Ed25519 signature over the exact bytes of
`manifest.json`. The manifest records the export public key and declares
`"encryption": "none"` because ADR 0001 defers envelope encryption and MOA stores
redacted graph-memory text at ingestion time.

## Contents

The tarball contains:

- `facts.jsonl`
- `entities.jsonl`
- `relationships.jsonl`
- `embeddings.jsonl`
- `skills.jsonl`
- `skill_addenda.jsonl`
- `changelog.jsonl`
- `README.md`
- `manifest.json`
- `manifest.sig`

Vectors are exported for provenance. Treat the archive as PHI-adjacent even
though the memory text is already redacted.

## Audit trail

Each successful export writes `op='export'` to `moa.graph_changelog` with the
reason, subject user id, artifact counts, approver id, and approval token JTI.
M22 pgaudit captures the underlying reads and changelog insert in the Postgres
audit log stream.

## Optional PGP wrapping

Use `--pgp-recipient <recipient.pub>` to produce an additional `.gpg` file next
to the tarball. Deliver only through an approved secure channel.

## Operational checks

1. Confirm the approval ticket and subject identity.
2. Generate a short-lived approval JWT and record its JTI in the ticket.
3. Run the export on an admin workstation with the ops signing key available.
4. Verify the manifest signature before delivery.
5. Confirm a matching `op='export'` changelog row exists.
6. Store delivery evidence in the ticket; do not paste archive contents into
   chat or issue trackers.

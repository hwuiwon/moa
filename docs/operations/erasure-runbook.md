# Subject erasure runbook

MOA supports GDPR Article 17 erasure with a hard-purge-only command:

```sh
moa privacy erase \
  --workspace <workspace-uuid> \
  --user <subject-user-uuid> \
  --reason "GDPR Art.17 request <ticket>" \
  --approval-token <signed-platform-admin-jwt>
```

Use `--dry-run` first to list the candidate count and a sample of node ids
without writing graph, embedding, approval-JTI, or changelog rows.

## Authorization

The command requires an Ed25519-signed approval JWT with:

- `sub`: approver identifier
- `jti`: unique token id
- `exp`: expiration timestamp
- `op`: `erase`
- `subject_user_id`: the erased user UUID
- `role` or `roles`: includes `platform_admin`
- optional `workspace_id`: when present, it must match `--workspace`

The CLI verifies the token with `MOA_PRIVACY_APPROVAL_PUBLIC_KEY_HEX`. For
non-dry-run erasures with matching candidates, it records the JTI in
`moa.audit_jti_used` so the approval cannot be replayed.

## What gets erased

For every active `moa.node_index` row in the workspace whose `user_id` or
`properties_summary.user_id` matches the subject, MOA calls the graph
hard-purge path. That path deletes:

- the AGE vertex and attached edges
- the `moa.node_index` sidecar row
- associated `moa.embeddings` rows
- dependent `moa.skill_addendum` rows through the node foreign key

The operation does not decrypt data and has no crypto-shred mode. ADR 0001
deferred envelope encryption; erasure is hard-purge only.

## Audit trail

Each purged node leaves a redacted `op='erase'` changelog row with a redaction
marker and an audit metadata object containing the reason, approver id, approval
token JTI, subject user id, and workspace id. After at least one node is erased,
the CLI writes one summary `op='erase'` row targeting the subject user.

Re-running after all matching nodes are gone returns `erased_count: 0` and writes
no new changelog rows.

## Operational checks

1. Confirm the erasure ticket, subject identity, and workspace.
2. Run `moa privacy erase ... --dry-run` and attach the candidate count to the
   ticket.
3. Generate a short-lived approval JWT with `op='erase'`.
4. Run the non-dry-run command.
5. Confirm `erased_count` matches the approved candidate count.
6. Confirm a summary `op='erase'` changelog row exists for the subject.
7. Confirm a second run with a fresh approval token returns `erased_count: 0`.

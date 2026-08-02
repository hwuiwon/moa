# KMS Root-Key Rotation

MOA uses envelope encryption: Postgres stores per-subject key-encryption keys
(KEKs) wrapped by a deployment root key, while root-key material exists only in
the mounted `moa-kms-root-keys` Kubernetes Secret. Every orchestrator replica
and KMS maintenance Job must mount the same generation files read-only at
`/var/run/secrets/moa-kms/root-keys`. `moa-edge` must never mount this Secret.

Each Secret key is a generation name and each value is a base64-encoded 32-byte
key. Generation names are persisted with KEKs, so a rolling rotation must keep
old and new files mounted until every live KEK has moved and the old generation
has been retired. Never replace a file's contents in place or reuse a generation
name for different material.

## Provision the initial keyring

Generate the key outside the cluster with an approved secret-management system.
Provision it as a file-backed Secret without placing its contents in a manifest
or shell argument:

```bash
kubectl -n moa-system create secret generic moa-kms-root-keys \
  --from-file=primary=/secure/path/to/base64-primary
```

The production overlay expects that Secret to exist; it does not generate key
material. The local overlay deliberately generates the same Secret name from a
fixed, checked-in development-only file so local ciphertext survives restarts.
That development key is public and must never be used for shared or production
data.

## Rotate without downtime

Assume the active generation is `primary` and the new generation is
`2026-08`. Follow this order exactly:

1. Add the `2026-08` file to `moa-kms-root-keys` while retaining `primary`.
   Confirm every running pod can still see every generation referenced by a
   live KEK.
2. Change `MOA_KMS_REQUIRED_GENERATION` to `2026-08` and begin a rolling
   orchestrator update. New pods mount both files but remain unready because
   Postgres still selects `primary`; ready old pods continue serving traffic.
3. Update the opt-in Job manifest to require `2026-08`, then apply only the
   maintenance kustomization:

   ```bash
   kubectl apply -k k8s/jobs
   kubectl -n moa-system wait --for=condition=complete job/moa-kms-rewrap --timeout=3600s
   kubectl -n moa-system logs job/moa-kms-rewrap
   ```

   `kms-rewrap --batch-size 100` first activates the configured required
   generation and then moves live KEKs in bounded, resumable transactions until
   no old references remain. Concurrent Jobs are safe, and retrying after a
   failure is idempotent.
4. Once Postgres selects `2026-08`, new pods become ready and old pods become
   unready. Complete the rollout and verify all ready replicas report compatible
   KMS state.
5. After the new fleet is ready, add `--retire-generation primary` to the
   `moa-kms-rewrap` Job args, delete the completed Job object, and apply the jobs
   kustomization again. The resulting command is:

   ```text
   kms-rewrap --batch-size 100 --retire-generation primary
   ```

   This run rechecks and rewraps to zero before calling the KMS provider's
   guarded retirement operation. Retirement rejects active or still-referenced
   generations. Only after it succeeds may a later rollout remove the
   `primary` file from the Secret.

Kubernetes Jobs are immutable. Delete the completed `moa-kms-rewrap` Job before
applying it for a later rotation. This removes only the completed Job object;
the operation state and KEKs remain in Postgres.

Do not add the maintenance Job to `k8s/base` or an application overlay. The
operator must opt in to this write operation deliberately.

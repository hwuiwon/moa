# Step M24 — Right to erasure CLI (`moa privacy erase`) — hard-purge only

_Hard-purge a subject's data across AGE, sidecar, embeddings, and changelog (with audit-preserving redaction marker), atomically, with idempotency, dry-run, authorized approval, and full audit emit. No crypto-shred mode (envelope encryption deferred per ADR 0001)._

## 1 What this step is about

GDPR Art. 17 Right to Erasure requires a deterministic deletion path. M08 already exposes `hard_purge` (delete in AGE + node_index + embeddings; preserve a redacted audit row in `graph_changelog`). M24 wraps it in an admin CLI.

Original M24 had two modes (`hard` and `crypto`). With envelope encryption deferred (ADR 0001), there is no DEK to destroy and no `crypto` mode. The CLI has a single behavior: hard-purge, audit row preserved with redaction marker.

## 2 Files to read

- `crates/moa-memory/graph/src/write.rs` (M08 `hard_purge`)
- `crates/moa-cli/src/commands/privacy.rs` (M23 Export variant — extend it)
- `migrations/M22_pgaudit.sql`
- `docs/architecture/decisions/0001-envelope-encryption-deferred.md`

## 3 Goal

1. `moa privacy erase --workspace <wid> --user <uid> [--reason "..."] [--dry-run] --approval-token <jwt>`
2. Single mode: hard-purge per node via M08 `hard_purge`. No `--mode` flag.
3. Authorization: `platform_admin` only; signed approval token (same as M23).
4. Audit row written for every erased node (`op='erase'`, `audit_metadata={reason, approver_id, approval_token_jti}`).
5. Idempotent: re-running on already-erased nodes returns count=0 with no new changelog rows.

## 4 Rules

- **Idempotent.** Re-runs are safe and return 0 erased.
- **Atomic per node.** Each per-node erase is one transaction; batch is committed in chunks of 1000.
- **Dry-run.** Lists candidate nodes + counts but writes nothing (no graph changes, no changelog rows).
- **No --decrypt.** Erase never decrypts. Operates on `uid` matching only.
- **Op vocabulary.** Changelog `op = 'erase'` for application-driven hard-purge. The `op = 'crypto_shred'` value from the original M24 design is NOT added to the CHECK constraint.

## 5 Tasks

### 5a CLI subcommand and authz

```rust
#[derive(Subcommand)]
pub enum PrivacyCmd {
    Export { /* M23 */ },
    Erase {
        #[arg(long)] workspace: Uuid,
        #[arg(long)] user: Uuid,
        #[arg(long)] reason: String,
        #[arg(long, default_value_t = false)] dry_run: bool,
        #[arg(long)] approval_token: String,           // signed JWT, verified against KMS
    },
}
```

Authorization: verify approval_token signature against ops Ed25519 public key (KMS-backed). Reject if expired, replayed (jti seen before), or missing required claims (`sub`, `subject_user_id`, `op="erase"`).

### 5b Candidate enumeration

```sql
SELECT uid
  FROM moa.node_index
 WHERE workspace_id = $1
   AND (user_id = $2 OR properties_summary->>'user_id' = $2::text)
   AND valid_to IS NULL
 ORDER BY uid;
```

If `--dry-run`, print the count and a sample of `uid`s, then exit 0.

### 5c Erase loop

```rust
for chunk in candidates.chunks(1000) {
    for uid in chunk {
        // M08 hard_purge: tx-atomic across AGE + node_index + embeddings + changelog
        ctx.graph.hard_purge(*uid, &format!("erase:{}", jti)).await?;
    }
    // Optional: emit progress to stderr every chunk
}
```

`hard_purge` is responsible for:
- DETACH DELETE in AGE
- DELETE FROM `moa.node_index`
- DELETE FROM `moa.embeddings` (cascades via FK to `node_index.uid`)
- INSERT into `moa.graph_changelog` with `op='erase'`, payload containing redaction marker (blake3 hash of `properties_summary` for audit reconstruction without preserving the data itself)

### 5d Migration: extend op CHECK to include `'erase'` (and `'export'` from M23)

`migrations/M24_erase_ops.sql`:

```sql
-- Cumulative op CHECK after M23 added 'export' and M24 adds 'erase'.
ALTER TABLE moa.graph_changelog DROP CONSTRAINT IF EXISTS graph_changelog_op_check;
ALTER TABLE moa.graph_changelog ADD CONSTRAINT graph_changelog_op_check
  CHECK (op IN ('create','update','supersede','invalidate','erase','export'));
```

(Note: original M24 design also included `'crypto_shred'`. With M21 deferred, `'crypto_shred'` is intentionally NOT in this list.)

### 5e Audit emit

`hard_purge` already writes the per-node `op='erase'` changelog row. Additionally emit one summary row for the operation as a whole:

```rust
crate::changelog::write_and_bump(&mut tx, ChangelogRecord {
    workspace_id: Some(workspace), user_id: Some(user), scope: "workspace".into(),
    actor_id: Some(approver), actor_kind: "admin".into(),
    op: "erase".into(),
    target_kind: "user".into(),
    target_label: "User".into(),
    target_uid: user,
    payload: json!({"reason": reason, "erased_count": count}),
    pii_class: "phi".into(),
    audit_metadata: Some(json!({"approver": approver, "jti": jti})),
    cause_change_id: None,
}).await?;
```

This summary row is the single auditable artifact pointing back at the operation, distinct from the per-node erase rows.

## 6 Deliverables

- `crates/moa-cli/src/commands/privacy.rs` — Erase variant (~200 lines added).
- `migrations/M24_erase_ops.sql`.
- `docs/operations/erasure-runbook.md`.

## 7 Acceptance criteria

1. Post-erase, `SELECT COUNT(*) FROM moa.node_index WHERE uid = ANY($1)` is 0; AGE Cypher `MATCH (n) WHERE n.uid IN [...]` returns 0; `moa.embeddings` count for those uids is 0.
2. `moa.graph_changelog` has +N `op='erase'` rows (one per node) plus 1 summary row.
3. Idempotent: second invocation returns `erased_count=0`, no new changelog rows.
4. Dry-run: prints candidate count, writes nothing (changelog count unchanged).
5. RLS: cannot erase another workspace's nodes even with `platform_admin` (must SET LOCAL workspace_id).
6. Authz fails with clear message if approval token missing/expired/replayed.
7. 10K-node erase completes <60s on local laptop hardware.
8. CHECK constraint on `graph_changelog.op` allows `'erase'` and rejects `'crypto_shred'`.

## 8 Tests

```sh
cargo test -p moa-cli privacy_erase_basic
cargo test -p moa-cli privacy_erase_idempotent
cargo test -p moa-cli privacy_erase_dry_run
cargo test -p moa-cli privacy_erase_authz_required
cargo test -p moa-cli privacy_erase_jti_replay_blocked
cargo test -p moa-cli privacy_erase_cross_tenant_denied
cargo test -p moa-cli privacy_erase_crypto_shred_op_rejected   # asserts CHECK rejects 'crypto_shred'
```

## 9 Cleanup

- Remove any prior "delete user data" scripts that bypassed the changelog.
- Remove any DELETE statement on `moa.node_index` outside the `hard_purge` path.
- Confirm no leftover `crypto_shred` references in code or migrations:

```sh
rg "crypto_shred" crates/ migrations/   # expected: 0 hits (or only in docs/architecture/decisions/0001-...)
```

## 10 What's next

**M25 — Cross-tenant pen-test suite.** Now includes a redaction-bypass attack instead of the original KEK-substitution attack (since envelope encryption is deferred).

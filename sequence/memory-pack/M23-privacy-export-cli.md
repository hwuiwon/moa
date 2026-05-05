# Step M23 — `moa privacy export` CLI (GDPR Art. 15 subject access)

_Build the admin CLI that exports every fact, embedding, addendum, skill, and changelog row attributable to a given user across all workspaces, signs the manifest with Ed25519, and writes a tarball ready for delivery to the data subject._

## 1 What this step is about

GDPR Article 15 grants data subjects the right to a copy of their personal data. We need a one-command operation that gathers everything. Outputs are signed for tamper-evidence; a manifest enumerates every artifact + checksum.

> **Note**: With M21 envelope encryption deferred (see ADR 0001), there is no decryption path in the export. Facts are stored as redacted text from ingestion onward; the export emits them as-is. This is simpler than the original M23 design.

## 2 Files to read

- `docs/architecture/decisions/0001-envelope-encryption-deferred.md` (so you understand why no decrypt step)
- M06 `graph_changelog` (all attributable changes)
- M18/M19 (skills, addenda)
- M22 (audit obligations — the export operation itself MUST be audited)

## 3 Goal

`moa privacy export --workspace <wid> --user <uid> --reason "..." --approval-token <jwt> --out tarball.tgz`

Produces:

```
export/
├── manifest.json              # files + sha256 + signature
├── manifest.sig               # Ed25519 signature over manifest.json
├── facts.jsonl                # Fact / Lesson / Decision / Incident nodes (redacted text as stored)
├── entities.jsonl
├── relationships.jsonl
├── embeddings.jsonl           # vectors as JSON arrays (provenance only)
├── skills.jsonl
├── skill_addenda.jsonl
├── changelog.jsonl            # full audit trail filtered to user
└── README.md                  # human-readable summary
```

## 4 Rules

- **Authorized only**: requires `platform_admin` role + signed approval token (verified against KMS).
- **Workspace + cross-workspace**: by default exports for a given user across every workspace they appear in (admin-only); single-workspace mode is an option.
- **No decryption step**: facts are stored as redacted text; emit as-is. No `maybe_decrypt` calls.
- **Tarball still treated as PHI-adjacent**: the redacted output may still contain quasi-identifiers. Deliver via secure channel; CLI offers PGP-encrypt for the recipient.
- **Audit row**: every export emits an `op='export'` changelog row capturing the request and approval token jti.
- **Manifest signature** uses an ops Ed25519 keypair stored in KMS.

## 5 Tasks

### 5a CLI subcommand

`crates/moa-cli/src/commands/privacy.rs`:

```rust
#[derive(Subcommand)]
pub enum PrivacyCmd {
    Export {
        #[arg(long)] workspace: Option<Uuid>,    // None => all workspaces
        #[arg(long)] user: Uuid,
        #[arg(long)] reason: String,
        #[arg(long)] approval_token: String,     // JWT
        #[arg(long)] out: PathBuf,
        #[arg(long)] pgp_recipient: Option<PathBuf>,
    },
    Erase { /* ... M24 */ },
}
```

### 5b Approval token verification

Verify a JWT signed by the ops keypair against a KMS-held public key. Reject if missing/expired/replayed (jti seen before in `moa.audit_jti_used`).

### 5c Collectors

Each collector streams to a `.jsonl` file in the export directory.

```rust
async fn collect_facts(ws: Option<Uuid>, user: Uuid, ctx: &Ctx, out: &mut impl Write) -> Result<usize> {
    // Query node_index where workspace matches AND (user_id = user OR
    // properties_summary contains reference to user). Stream rows.
    // No decryption — store the row as-is.
    let mut count = 0;
    let rows = sqlx::query!(
        r#"SELECT uid, label, name, properties_summary, pii_class, valid_from, valid_to,
                  created_at, last_accessed_at
           FROM moa.node_index
           WHERE valid_to IS NULL
             AND ($1::uuid IS NULL OR workspace_id = $1)
             AND (user_id = $2 OR properties_summary->>'user_id' = $2::text)"#,
        ws, user,
    ).fetch_all(&ctx.pool).await?;
    for r in rows {
        let line = serde_json::json!({
            "uid": r.uid, "label": r.label, "name": r.name,
            "properties_summary": r.properties_summary,
            "pii_class": r.pii_class,
            "valid_from": r.valid_from, "valid_to": r.valid_to,
            "created_at": r.created_at,
        });
        writeln!(out, "{}", serde_json::to_string(&line)?)?;
        count += 1;
    }
    Ok(count)
}

// Similar for: entities, relationships, embeddings, skills, addenda, changelog.
```

Each collector:
- Sets `SET LOCAL ROLE moa_app`, GUCs for the workspace and `scope_tier` so RLS policies apply.
- For admin override (cross-workspace), uses `moa_promoter` only after explicit log entry.

### 5d Manifest + signature

```rust
async fn write_manifest(out_dir: &Path, signer: &dyn Ed25519Signer, subject: Uuid) -> Result<()> {
    let mut entries = vec![];
    for f in fs::read_dir(out_dir)? {
        let f = f?;
        let bytes = fs::read(f.path())?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        entries.push(json!({
            "name": f.file_name().to_string_lossy(),
            "size": bytes.len(),
            "blake3": hash,
        }));
    }
    let manifest = json!({
        "version": 1,
        "created_at": Utc::now().to_rfc3339(),
        "subject_user_id": subject.to_string(),
        "encryption": "none",   // explicit: redaction-only privacy model
        "files": entries,
    });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    fs::write(out_dir.join("manifest.json"), &manifest_bytes)?;
    let sig = signer.sign(&manifest_bytes).await?;
    fs::write(out_dir.join("manifest.sig"), sig)?;
    Ok(())
}
```

### 5e Tarball + optional PGP

```rust
async fn finalize(out_dir: &Path, target: &Path, pgp: Option<&Path>) -> Result<()> {
    tar_gz(out_dir, target)?;
    if let Some(recipient) = pgp {
        // shell out to gpg --encrypt --recipient-file recipient.pub --output target.gpg target
    }
    Ok(())
}
```

### 5f Audit emit

```rust
crate::changelog::write_and_bump(&mut tx, ChangelogRecord {
    workspace_id: ws, user_id: Some(user), scope: "workspace".into(),
    actor_id: Some(approver), actor_kind: "admin".into(),
    op: "export".into(), target_kind: "user".into(), target_label: "User".into(),
    target_uid: user, payload: json!({"reason": reason, "files": file_count}),
    pii_class: "phi".into(),
    audit_metadata: Some(json!({"approval_token_jti": jti})),
    cause_change_id: None,
}).await?;
```

### 5g README.md inside the tarball

Generate a short human-readable summary explaining:
- What the recipient is looking at (per GDPR Art. 15).
- That data is redacted (no original PHI included).
- Manifest signature instructions (how to verify).
- Contact for follow-up questions.

## 6 Deliverables

- `crates/moa-cli/src/commands/privacy.rs` (Export variant) (~300 lines).
- Manifest signer using KMS-backed Ed25519.
- `docs/operations/subject-access-runbook.md`.

## 7 Acceptance criteria

1. CLI command produces a tarball containing every artifact for the user.
2. Manifest signature verifies with ops public key.
3. Manifest declares `"encryption": "none"`.
4. Audit row created; pgaudit captures the operation.
5. Without approval token, command exits 2 with "approval token required".
6. Replayed approval token (same `jti`) is rejected.

## 8 Tests

```sh
cargo test -p moa-cli privacy_export_round_trip
cargo test -p moa-cli privacy_export_authz_required
cargo test -p moa-cli privacy_export_jti_replay_blocked
moa privacy export --workspace <wid> --user <uid> --reason "GDPR Art.15 request" --approval-token <jwt> --out /tmp/sub.tgz
tar -tzf /tmp/sub.tgz
```

## 9 Cleanup

- Remove any prior ad-hoc "export user data" scripts.
- Remove any code path that wrote subject-access output to non-signed formats.

## 10 What's next

**M24 — Right to erasure CLI (`moa privacy erase`).** Hard-purge only since envelope encryption is deferred.

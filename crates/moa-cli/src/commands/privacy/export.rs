//! Privacy export command implementation.

use super::*;

/// Arguments for `moa privacy export`.
#[derive(Debug, Args)]
pub struct Args {
    /// Optional workspace id. Omit to export all workspaces visible to the admin token.
    #[arg(long)]
    pub workspace: Option<String>,
    /// Subject user id for the data export.
    #[arg(long)]
    pub user: Uuid,
    /// Administrative reason recorded in the audit trail.
    #[arg(long)]
    pub reason: String,
    /// Signed platform-admin approval token.
    #[arg(long)]
    pub approval_token: String,
    /// Target `.tgz` path.
    #[arg(long)]
    pub out: PathBuf,
    /// Optional PGP recipient public key file used to encrypt the generated tarball.
    #[arg(long)]
    pub pgp_recipient: Option<PathBuf>,
}

pub(super) async fn run(config: &MoaConfig, args: Args) -> Result<String> {
    if args.reason.trim().is_empty() {
        bail!("--reason is required");
    }

    let session_store = PostgresSessionStore::from_admin_config(config)
        .await
        .context("opening admin session store")?;
    let pool = session_store.pool().clone();
    let subject_user_id = args.user.to_string();
    let verifier = ApprovalTokenVerifier::from_env()?;
    let claims = verifier.verify(
        &args.approval_token,
        "export",
        &subject_user_id,
        args.workspace.as_deref(),
    )?;
    consume_approval_jti(&pool, &claims).await?;

    let signer = Ed25519ManifestSigner::from_env()?;
    let export_dir = create_export_dir(&args.out).await?;
    let ctx = ExportContext {
        pool,
        workspace: args.workspace.clone(),
        subject_user: args.user,
        subject_user_id,
        reason: args.reason.clone(),
        claims,
    };

    let result = async {
        let mut counts = BTreeMap::new();
        counts.insert("facts", collect_facts(&ctx, &export_dir).await?);
        counts.insert("entities", collect_entities(&ctx, &export_dir).await?);
        counts.insert(
            "relationships",
            collect_relationships(&ctx, &export_dir).await?,
        );
        counts.insert("embeddings", collect_embeddings(&ctx, &export_dir).await?);
        counts.insert("skills", collect_skills(&ctx, &export_dir).await?);
        counts.insert(
            "skill_addenda",
            collect_skill_addenda(&ctx, &export_dir).await?,
        );
        write_export_readme(&ctx, &counts, &export_dir).await?;
        emit_export_audit(&ctx, &counts).await?;
        counts.insert("changelog", collect_changelog(&ctx, &export_dir).await?);
        write_manifest(&export_dir, &signer, &ctx, &counts).await?;
        finalize_archive(&export_dir, &args.out, args.pgp_recipient.as_deref()).await?;
        Ok::<_, anyhow::Error>(counts)
    }
    .await;

    let cleanup = fs::remove_dir_all(&export_dir).await;
    let counts = result?;
    if let Err(error) = cleanup {
        tracing::warn!(path = %export_dir.display(), %error, "failed to remove privacy export staging directory");
    }

    Ok(format!(
        "privacy export written\nsubject_user_id: {}\nworkspace: {}\narchive: {}\nfiles: {}\n",
        ctx.subject_user_id,
        ctx.workspace.as_deref().unwrap_or("all"),
        args.out.display(),
        counts.len() + 3
    ))
}

#[derive(Debug)]
pub(super) struct ExportContext {
    pub(super) pool: PgPool,
    pub(super) workspace: Option<String>,
    pub(super) subject_user: Uuid,
    pub(super) subject_user_id: String,
    pub(super) reason: String,
    pub(super) claims: ApprovalClaims,
}

async fn create_export_dir(target: &Path) -> Result<PathBuf> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .await
        .with_context(|| format!("creating {}", parent.display()))?;
    let dir = parent.join(format!(".moa-privacy-export-{}", Uuid::now_v7()));
    fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

async fn collect_facts(ctx: &ExportContext, export_dir: &Path) -> Result<usize> {
    collect_nodes(
        ctx,
        export_dir.join("facts.jsonl"),
        &["Fact", "Lesson", "Decision", "Incident"],
    )
    .await
}

async fn collect_entities(ctx: &ExportContext, export_dir: &Path) -> Result<usize> {
    collect_nodes(
        ctx,
        export_dir.join("entities.jsonl"),
        &["Entity", "Concept", "Source"],
    )
    .await
}

async fn collect_nodes(ctx: &ExportContext, path: PathBuf, labels: &[&str]) -> Result<usize> {
    let label_filter = labels
        .iter()
        .map(|label| (*label).to_string())
        .collect::<Vec<_>>();
    let mut tx = begin_audited_read(&ctx.pool).await?;
    let rows = sqlx::query_scalar::<_, Value>(
        r#"
        SELECT jsonb_build_object(
            'uid', uid,
            'label', label,
            'workspace_id', workspace_id,
            'user_id', user_id,
            'scope', scope,
            'name', name,
            'properties_summary', properties_summary,
            'pii_class', pii_class,
            'confidence', confidence,
            'valid_from', valid_from,
            'valid_to', valid_to,
            'created_at', created_at,
            'last_accessed_at', last_accessed_at
        )
        FROM moa.node_index
        WHERE valid_to IS NULL
          AND label = ANY($3)
          AND ($1::text IS NULL OR workspace_id = $1)
          AND (
              user_id = $2
              OR properties_summary->>'user_id' = $2
              OR properties_summary::text LIKE ('%' || $2 || '%')
          )
        ORDER BY workspace_id NULLS FIRST, label, name, uid
        "#,
    )
    .bind(ctx.workspace.as_deref())
    .bind(&ctx.subject_user_id)
    .bind(label_filter)
    .fetch_all(&mut *tx)
    .await
    .context("collecting node rows")?;
    tx.commit().await.context("committing node export read")?;
    write_jsonl(path, &rows).await
}

async fn collect_relationships(ctx: &ExportContext, export_dir: &Path) -> Result<usize> {
    let mut tx = begin_audited_read(&ctx.pool).await?;
    let rows = sqlx::query_scalar::<_, Value>(
        r#"
        SELECT jsonb_build_object(
            'change_id', change_id,
            'workspace_id', workspace_id,
            'user_id', user_id,
            'scope', scope,
            'actor_id', actor_id,
            'actor_kind', actor_kind,
            'op', op,
            'target_kind', target_kind,
            'target_label', target_label,
            'target_uid', target_uid,
            'payload', payload,
            'pii_class', pii_class,
            'audit_metadata', audit_metadata,
            'cause_change_id', cause_change_id,
            'created_at', created_at
        )
        FROM moa.graph_changelog
        WHERE target_kind = 'edge'
          AND ($1::text IS NULL OR workspace_id = $1)
          AND (
              user_id = $2
              OR actor_id = $2
              OR payload::text LIKE ('%' || $2 || '%')
              OR audit_metadata->>'subject_user_id' = $2
          )
        ORDER BY created_at, change_id
        "#,
    )
    .bind(ctx.workspace.as_deref())
    .bind(&ctx.subject_user_id)
    .fetch_all(&mut *tx)
    .await
    .context("collecting relationship changelog rows")?;
    tx.commit()
        .await
        .context("committing relationship export read")?;
    write_jsonl(export_dir.join("relationships.jsonl"), &rows).await
}

async fn collect_embeddings(ctx: &ExportContext, export_dir: &Path) -> Result<usize> {
    let mut tx = begin_audited_read(&ctx.pool).await?;
    let rows = sqlx::query_scalar::<_, Value>(
        r#"
        SELECT jsonb_build_object(
            'uid', e.uid,
            'workspace_id', e.workspace_id,
            'user_id', e.user_id,
            'scope', e.scope,
            'label', e.label,
            'pii_class', e.pii_class,
            'embedding_model', e.embedding_model,
            'embedding_model_version', e.embedding_model_version,
            'embedding', (e.embedding::text)::jsonb,
            'valid_to', e.valid_to,
            'created_at', e.created_at
        )
        FROM moa.embeddings e
        JOIN moa.node_index n ON n.uid = e.uid
        WHERE e.valid_to IS NULL
          AND n.valid_to IS NULL
          AND ($1::text IS NULL OR e.workspace_id = $1)
          AND (
              e.user_id = $2
              OR n.user_id = $2
              OR n.properties_summary->>'user_id' = $2
              OR n.properties_summary::text LIKE ('%' || $2 || '%')
          )
        ORDER BY e.workspace_id NULLS FIRST, e.label, e.uid
        "#,
    )
    .bind(ctx.workspace.as_deref())
    .bind(&ctx.subject_user_id)
    .fetch_all(&mut *tx)
    .await
    .context("collecting embedding rows")?;
    tx.commit()
        .await
        .context("committing embedding export read")?;
    write_jsonl(export_dir.join("embeddings.jsonl"), &rows).await
}

async fn collect_skills(ctx: &ExportContext, export_dir: &Path) -> Result<usize> {
    let mut tx = begin_audited_read(&ctx.pool).await?;
    let rows = sqlx::query_scalar::<_, Value>(
        r#"
        SELECT jsonb_build_object(
            'skill_uid', skill_uid,
            'workspace_id', workspace_id,
            'user_id', user_id,
            'scope', scope,
            'name', name,
            'description', description,
            'body', body,
            'body_hash_hex', encode(body_hash, 'hex'),
            'version', version,
            'previous_skill_uid', previous_skill_uid,
            'tags', tags,
            'valid_to', valid_to,
            'created_at', created_at,
            'updated_at', updated_at
        )
        FROM moa.skill
        WHERE valid_to IS NULL
          AND ($1::text IS NULL OR workspace_id = $1)
          AND (
              user_id = $2
              OR body LIKE ('%' || $2 || '%')
              OR description LIKE ('%' || $2 || '%')
          )
        ORDER BY workspace_id NULLS FIRST, scope, name, version
        "#,
    )
    .bind(ctx.workspace.as_deref())
    .bind(&ctx.subject_user_id)
    .fetch_all(&mut *tx)
    .await
    .context("collecting skill rows")?;
    tx.commit().await.context("committing skill export read")?;
    write_jsonl(export_dir.join("skills.jsonl"), &rows).await
}

async fn collect_skill_addenda(ctx: &ExportContext, export_dir: &Path) -> Result<usize> {
    let mut tx = begin_audited_read(&ctx.pool).await?;
    let rows = sqlx::query_scalar::<_, Value>(
        r#"
        SELECT jsonb_build_object(
            'addendum_uid', a.addendum_uid,
            'skill_uid', a.skill_uid,
            'linked_lesson_uid', a.linked_lesson_uid,
            'workspace_id', a.workspace_id,
            'user_id', a.user_id,
            'scope', a.scope,
            'summary', a.summary,
            'created_at', a.created_at,
            'valid_to', a.valid_to
        )
        FROM moa.skill_addendum a
        LEFT JOIN moa.node_index n ON n.uid = a.linked_lesson_uid
        WHERE a.valid_to IS NULL
          AND ($1::text IS NULL OR a.workspace_id = $1)
          AND (
              a.user_id = $2
              OR a.summary LIKE ('%' || $2 || '%')
              OR n.user_id = $2
              OR n.properties_summary->>'user_id' = $2
              OR n.properties_summary::text LIKE ('%' || $2 || '%')
          )
        ORDER BY a.workspace_id NULLS FIRST, a.created_at, a.addendum_uid
        "#,
    )
    .bind(ctx.workspace.as_deref())
    .bind(&ctx.subject_user_id)
    .fetch_all(&mut *tx)
    .await
    .context("collecting skill addendum rows")?;
    tx.commit()
        .await
        .context("committing skill addendum export read")?;
    write_jsonl(export_dir.join("skill_addenda.jsonl"), &rows).await
}

async fn collect_changelog(ctx: &ExportContext, export_dir: &Path) -> Result<usize> {
    let mut tx = begin_audited_read(&ctx.pool).await?;
    let rows = sqlx::query_scalar::<_, Value>(
        r#"
        SELECT jsonb_build_object(
            'change_id', change_id,
            'workspace_id', workspace_id,
            'user_id', user_id,
            'scope', scope,
            'actor_id', actor_id,
            'actor_kind', actor_kind,
            'op', op,
            'target_kind', target_kind,
            'target_label', target_label,
            'target_uid', target_uid,
            'payload', payload,
            'redaction_marker', redaction_marker,
            'pii_class', pii_class,
            'audit_metadata', audit_metadata,
            'cause_change_id', cause_change_id,
            'created_at', created_at
        )
        FROM moa.graph_changelog
        WHERE ($1::text IS NULL OR workspace_id = $1)
          AND (
              user_id = $2
              OR actor_id = $2
              OR target_uid::text = $2
              OR payload::text LIKE ('%' || $2 || '%')
              OR audit_metadata->>'subject_user_id' = $2
          )
        ORDER BY created_at, change_id
        "#,
    )
    .bind(ctx.workspace.as_deref())
    .bind(&ctx.subject_user_id)
    .fetch_all(&mut *tx)
    .await
    .context("collecting changelog rows")?;
    tx.commit()
        .await
        .context("committing changelog export read")?;
    write_jsonl(export_dir.join("changelog.jsonl"), &rows).await
}

async fn begin_audited_read(pool: &PgPool) -> Result<sqlx::Transaction<'_, sqlx::Postgres>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL ROLE moa_auditor")
        .execute(&mut *tx)
        .await
        .context("assuming moa_auditor role")?;
    Ok(tx)
}

async fn write_jsonl(path: PathBuf, rows: &[Value]) -> Result<usize> {
    let mut file = fs::File::create(&path)
        .await
        .with_context(|| format!("creating {}", path.display()))?;
    for row in rows {
        file.write_all(serde_json::to_string(row)?.as_bytes())
            .await
            .with_context(|| format!("writing {}", path.display()))?;
        file.write_all(b"\n")
            .await
            .with_context(|| format!("writing {}", path.display()))?;
    }
    file.flush()
        .await
        .with_context(|| format!("flushing {}", path.display()))?;
    Ok(rows.len())
}

pub(super) async fn write_export_readme(
    ctx: &ExportContext,
    counts: &BTreeMap<&'static str, usize>,
    export_dir: &Path,
) -> Result<()> {
    let mut lines = Vec::new();
    lines.push("# MOA subject access export".to_string());
    lines.push(String::new());
    lines.push(format!("Created at: {}", Utc::now().to_rfc3339()));
    lines.push(format!("Subject user id: {}", ctx.subject_user_id));
    lines.push(format!(
        "Workspace: {}",
        ctx.workspace.as_deref().unwrap_or("all")
    ));
    lines.push(format!("Reason: {}", ctx.reason));
    lines.push(String::new());
    lines.push("This archive contains MOA graph memory, skills, addenda, embeddings, and audit rows attributable to the subject user for a GDPR Article 15 subject access request.".to_string());
    lines.push("MOA stores redacted graph-memory text after ingestion. This export does not decrypt or restore original PHI; it emits the persisted redacted data as stored.".to_string());
    lines.push("The archive may still contain quasi-identifiers and should be delivered only through an approved secure channel.".to_string());
    lines.push(String::new());
    lines.push("## Row counts".to_string());
    for (name, count) in counts {
        lines.push(format!("- {name}: {count}"));
    }
    lines.push(String::new());
    lines.push("## Manifest verification".to_string());
    lines.push("Verify `manifest.sig` as an Ed25519 signature over the exact bytes of `manifest.json` using the ops export public key recorded in the manifest.".to_string());
    lines.push(String::new());
    lines.push(
        "Contact the MOA platform operations team for follow-up questions or corrections."
            .to_string(),
    );
    lines.push(String::new());

    fs::write(export_dir.join("README.md"), lines.join("\n"))
        .await
        .context("writing export README")?;
    Ok(())
}

async fn emit_export_audit(
    ctx: &ExportContext,
    counts: &BTreeMap<&'static str, usize>,
) -> Result<()> {
    let mut tx = ctx.pool.begin().await.context("starting export audit tx")?;
    let scope = if ctx.workspace.is_some() {
        "workspace"
    } else {
        "global"
    };
    let file_count = counts.len() + 4;
    write_and_bump(
        &mut tx,
        ChangelogRecord {
            workspace_id: ctx.workspace.clone(),
            user_id: None,
            scope: scope.to_string(),
            actor_id: Some(ctx.claims.sub.clone()),
            actor_kind: "admin".to_string(),
            op: "export".to_string(),
            target_kind: "user".to_string(),
            target_label: "User".to_string(),
            target_uid: ctx.subject_user,
            payload: json!({
                "reason": ctx.reason,
                "subject_user_id": ctx.subject_user_id,
                "workspace": ctx.workspace.as_deref(),
                "artifact_counts": counts,
                "files": file_count,
            }),
            redaction_marker: None,
            pii_class: "phi".to_string(),
            audit_metadata: Some(json!({
                "approval_token_jti": ctx.claims.jti.as_str(),
                "approval_token_sub": ctx.claims.sub.as_str(),
                "subject_user_id": ctx.subject_user_id,
                "op": "export",
            })),
            cause_change_id: None,
        },
    )
    .await
    .context("writing export audit changelog row")?;
    tx.commit().await.context("committing export audit tx")?;
    Ok(())
}

#[derive(Debug, Serialize)]
struct Manifest<'a> {
    version: u8,
    created_at: String,
    subject_user_id: &'a str,
    workspace: Option<&'a str>,
    encryption: &'static str,
    signature: ManifestSignature<'a>,
    files: Vec<ManifestFile>,
    counts: BTreeMap<&'static str, usize>,
}

#[derive(Debug, Serialize)]
struct ManifestSignature<'a> {
    algorithm: &'static str,
    signature_file: &'static str,
    key_id: &'a str,
    public_key_hex: String,
}

#[derive(Debug, Serialize)]
struct ManifestFile {
    name: String,
    size: u64,
    sha256: String,
    blake3: String,
}

pub(super) async fn write_manifest(
    export_dir: &Path,
    signer: &Ed25519ManifestSigner,
    ctx: &ExportContext,
    counts: &BTreeMap<&'static str, usize>,
) -> Result<()> {
    let mut files = Vec::new();
    let mut entries = fs::read_dir(export_dir)
        .await
        .with_context(|| format!("reading {}", export_dir.display()))?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !entry.file_type().await?.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name == "manifest.json" || name == "manifest.sig" {
            continue;
        }
        let bytes = fs::read(&path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        files.push(ManifestFile {
            name: name.to_string(),
            size: u64::try_from(bytes.len()).context("manifest file size overflow")?,
            sha256: sha256_hex(&bytes),
            blake3: blake3::hash(&bytes).to_hex().to_string(),
        });
    }
    files.sort_by(|left, right| left.name.cmp(&right.name));

    let manifest = Manifest {
        version: 1,
        created_at: Utc::now().to_rfc3339(),
        subject_user_id: &ctx.subject_user_id,
        workspace: ctx.workspace.as_deref(),
        encryption: "none",
        signature: ManifestSignature {
            algorithm: "Ed25519",
            signature_file: "manifest.sig",
            key_id: signer.key_id(),
            public_key_hex: signer.public_key_hex(),
        },
        files,
        counts: counts.clone(),
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    fs::write(export_dir.join("manifest.json"), &manifest_bytes)
        .await
        .context("writing manifest.json")?;
    fs::write(
        export_dir.join("manifest.sig"),
        signer.sign(&manifest_bytes)?,
    )
    .await
    .context("writing manifest.sig")?;
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

pub(super) async fn finalize_archive(
    export_dir: &Path,
    target: &Path,
    pgp: Option<&Path>,
) -> Result<()> {
    let export_dir = export_dir.to_path_buf();
    let target = target.to_path_buf();
    let archive_target = target.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let file = std::fs::File::create(&archive_target)
            .with_context(|| format!("creating {}", archive_target.display()))?;
        let encoder = GzEncoder::new(file, Compression::default());
        let mut archive = Builder::new(encoder);
        archive
            .append_dir_all("export", &export_dir)
            .context("writing export archive")?;
        let encoder = archive.into_inner().context("finishing tar archive")?;
        encoder.finish().context("finishing gzip archive")?;
        Ok(())
    })
    .await
    .context("joining archive writer")??;

    if let Some(recipient) = pgp {
        encrypt_with_gpg(&target, recipient).await?;
    }

    Ok(())
}

async fn encrypt_with_gpg(target: &Path, recipient: &Path) -> Result<()> {
    let output = target.with_extension(format!(
        "{}.gpg",
        target
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("tgz")
    ));
    let status = Command::new("gpg")
        .arg("--batch")
        .arg("--yes")
        .arg("--encrypt")
        .arg("--recipient-file")
        .arg(recipient)
        .arg("--output")
        .arg(&output)
        .arg(target)
        .status()
        .await
        .context("running gpg")?;
    if !status.success() {
        bail!("gpg encryption failed with status {status}");
    }
    Ok(())
}

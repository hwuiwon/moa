//! Privacy administration CLI commands.

use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD as BASE64_STANDARD, URL_SAFE_NO_PAD};
use chrono::{TimeZone, Utc};
use clap::{Args, Subcommand};
use ed25519_dalek::{Signature, Signer as DalekSigner, SigningKey, Verifier, VerifyingKey};
use flate2::Compression;
use flate2::write::GzEncoder;
use moa_core::{MoaConfig, ScopeContext, ScopedConn, UserId, WorkspaceId};
use moa_memory_graph::{
    AgeGraphStore, ChangelogRecord, write::hard_purge_with_audit, write_and_bump,
};
use moa_session::PostgresSessionStore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tar::Builder;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use uuid::Uuid;

const APPROVAL_PUBLIC_KEY_ENV: &str = "MOA_PRIVACY_APPROVAL_PUBLIC_KEY_HEX";
const APPROVAL_PUBLIC_KEY_FALLBACK_ENV: &str = "MOA_PRIVACY_APPROVAL_PUBLIC_KEY";
const EXPORT_SIGNING_KEY_ENV: &str = "MOA_PRIVACY_EXPORT_SIGNING_KEY_HEX";
const EXPORT_SIGNING_KEY_FALLBACK_ENV: &str = "MOA_PRIVACY_EXPORT_SIGNING_KEY";
const EXPORT_SIGNING_KEY_ID_ENV: &str = "MOA_PRIVACY_EXPORT_SIGNING_KEY_ID";
const ERASE_CHUNK_SIZE: usize = 1000;
const ERASE_SAMPLE_LIMIT: usize = 20;

/// Privacy administration CLI commands.
#[derive(Debug, Subcommand)]
pub enum PrivacyCommand {
    /// Exports all personal graph memory data for one subject user.
    Export(PrivacyExportArgs),
    /// Hard-purges all graph memory attributable to one subject user in one workspace.
    Erase(PrivacyEraseArgs),
}

/// Arguments for `moa privacy export`.
#[derive(Debug, Args)]
pub struct PrivacyExportArgs {
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

/// Arguments for `moa privacy erase`.
#[derive(Debug, Args)]
pub struct PrivacyEraseArgs {
    /// Workspace containing the subject data to erase.
    #[arg(long)]
    pub workspace: Uuid,
    /// Subject user id for the erasure request.
    #[arg(long)]
    pub user: Uuid,
    /// Administrative reason recorded in the audit trail.
    #[arg(long)]
    pub reason: String,
    /// Lists candidate nodes without writing graph or changelog rows.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    /// Signed platform-admin approval token.
    #[arg(long)]
    pub approval_token: String,
}

/// Runs one privacy CLI command and returns a human-readable report.
pub async fn handle_privacy_command(config: &MoaConfig, command: PrivacyCommand) -> Result<String> {
    match command {
        PrivacyCommand::Export(args) => export_privacy_archive(config, args).await,
        PrivacyCommand::Erase(args) => erase_privacy_subject(config, args).await,
    }
}

async fn export_privacy_archive(config: &MoaConfig, args: PrivacyExportArgs) -> Result<String> {
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
struct ExportContext {
    pool: PgPool,
    workspace: Option<String>,
    subject_user: Uuid,
    subject_user_id: String,
    reason: String,
    claims: ApprovalClaims,
}

#[derive(Debug)]
struct EraseContext {
    pool: PgPool,
    workspace_id: String,
    subject_user: Uuid,
    subject_user_id: String,
    reason: String,
    claims: ApprovalClaims,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct EraseCandidate {
    uid: Uuid,
    label: String,
    name: String,
    pii_class: String,
}

async fn erase_privacy_subject(config: &MoaConfig, args: PrivacyEraseArgs) -> Result<String> {
    if args.reason.trim().is_empty() {
        bail!("--reason is required");
    }

    let session_store = PostgresSessionStore::from_admin_config(config)
        .await
        .context("opening admin session store")?;
    let pool = session_store.pool().clone();
    let workspace_id = args.workspace.to_string();
    let subject_user_id = args.user.to_string();
    let verifier = ApprovalTokenVerifier::from_env()?;
    let claims = verifier.verify(
        &args.approval_token,
        "erase",
        &subject_user_id,
        Some(&workspace_id),
    )?;
    let ctx = EraseContext {
        pool,
        workspace_id,
        subject_user: args.user,
        subject_user_id,
        reason: args.reason.clone(),
        claims,
    };

    execute_privacy_erase(ctx, args.dry_run).await
}

async fn execute_privacy_erase(ctx: EraseContext, dry_run: bool) -> Result<String> {
    let candidates = enumerate_erase_candidates(&ctx).await?;

    if dry_run {
        return Ok(format_erase_report(&ctx, &candidates, 0, true));
    }

    consume_approval_jti(&ctx.pool, &ctx.claims).await?;

    if candidates.is_empty() {
        return Ok(format_erase_report(&ctx, &candidates, 0, false));
    }

    let graph = erase_graph_store(&ctx.pool, &ctx.workspace_id, &ctx.subject_user_id);
    let mut erased_count = 0usize;
    for chunk in candidates.chunks(ERASE_CHUNK_SIZE) {
        for candidate in chunk {
            let metadata = erase_audit_metadata(&ctx);
            hard_purge_with_audit(
                &graph,
                candidate.uid,
                &format!("erase:{}", ctx.claims.jti),
                Some(metadata),
            )
            .await
            .with_context(|| format!("hard-purging memory node {}", candidate.uid))?;
            erased_count += 1;
        }
    }
    emit_erase_summary(&ctx, erased_count).await?;

    Ok(format_erase_report(&ctx, &candidates, erased_count, false))
}

fn erase_graph_store(pool: &PgPool, workspace_id: &str, subject_user_id: &str) -> AgeGraphStore {
    let scope = ScopeContext::user(WorkspaceId::new(workspace_id), UserId::new(subject_user_id));
    AgeGraphStore::scoped_for_app_role(pool.clone(), scope)
}

async fn enumerate_erase_candidates(ctx: &EraseContext) -> Result<Vec<EraseCandidate>> {
    let mut tx = begin_app_scoped_tx(&ctx.pool, &ctx.workspace_id, &ctx.subject_user_id).await?;
    let rows = sqlx::query_as::<_, EraseCandidate>(
        r#"
        SELECT uid, label, name, pii_class
        FROM moa.node_index
        WHERE workspace_id = $1
          AND valid_to IS NULL
          AND (
              user_id = $2
              OR properties_summary->>'user_id' = $2
          )
        ORDER BY uid
        "#,
    )
    .bind(&ctx.workspace_id)
    .bind(&ctx.subject_user_id)
    .fetch_all(tx.as_mut())
    .await
    .context("enumerating erasure candidates")?;
    tx.commit()
        .await
        .context("committing erasure candidate read")?;
    Ok(rows)
}

async fn begin_app_scoped_tx<'a>(
    pool: &'a PgPool,
    workspace_id: &str,
    subject_user_id: &str,
) -> Result<ScopedConn<'a>> {
    let scope = ScopeContext::user(WorkspaceId::new(workspace_id), UserId::new(subject_user_id));
    let mut tx = ScopedConn::begin(pool, &scope).await?;
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(tx.as_mut())
        .await
        .context("assuming moa_app role")?;
    Ok(tx)
}

fn erase_audit_metadata(ctx: &EraseContext) -> Value {
    json!({
        "reason": ctx.reason.as_str(),
        "approver_id": ctx.claims.sub.as_str(),
        "approval_token_jti": ctx.claims.jti.as_str(),
        "subject_user_id": ctx.subject_user_id.as_str(),
        "workspace_id": ctx.workspace_id.as_str(),
        "op": "erase",
    })
}

async fn emit_erase_summary(ctx: &EraseContext, erased_count: usize) -> Result<()> {
    let mut tx = begin_app_scoped_tx(&ctx.pool, &ctx.workspace_id, &ctx.subject_user_id)
        .await
        .context("starting erase summary tx")?;
    write_and_bump(
        tx.as_mut(),
        ChangelogRecord {
            workspace_id: Some(ctx.workspace_id.clone()),
            user_id: None,
            scope: "workspace".to_string(),
            actor_id: Some(ctx.claims.sub.clone()),
            actor_kind: "admin".to_string(),
            op: "erase".to_string(),
            target_kind: "user".to_string(),
            target_label: "User".to_string(),
            target_uid: ctx.subject_user,
            payload: json!({
                "reason": ctx.reason.as_str(),
                "subject_user_id": ctx.subject_user_id.as_str(),
                "erased_count": erased_count,
            }),
            redaction_marker: None,
            pii_class: "phi".to_string(),
            audit_metadata: Some(json!({
                "approver_id": ctx.claims.sub.as_str(),
                "approval_token_jti": ctx.claims.jti.as_str(),
                "subject_user_id": ctx.subject_user_id.as_str(),
                "workspace_id": ctx.workspace_id.as_str(),
                "op": "erase",
            })),
            cause_change_id: None,
        },
    )
    .await
    .context("writing erase summary changelog row")?;
    tx.commit()
        .await
        .context("committing erase summary changelog row")?;
    Ok(())
}

fn format_erase_report(
    ctx: &EraseContext,
    candidates: &[EraseCandidate],
    erased_count: usize,
    dry_run: bool,
) -> String {
    let mut report = String::new();
    if dry_run {
        report.push_str("privacy erase dry run\n");
    } else {
        report.push_str("privacy erase complete\n");
    }
    report.push_str(&format!("workspace: {}\n", ctx.workspace_id));
    report.push_str(&format!("subject_user_id: {}\n", ctx.subject_user_id));
    report.push_str(&format!("candidate_count: {}\n", candidates.len()));
    report.push_str(&format!("erased_count: {erased_count}\n"));

    if dry_run && !candidates.is_empty() {
        report.push_str("sample:\n");
        for candidate in candidates.iter().take(ERASE_SAMPLE_LIMIT) {
            report.push_str(&format!(
                "- {}\t{}\t{}\t{}\n",
                candidate.uid, candidate.label, candidate.name, candidate.pii_class
            ));
        }
    }

    report
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

async fn write_export_readme(
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

async fn write_manifest(
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

async fn finalize_archive(export_dir: &Path, target: &Path, pgp: Option<&Path>) -> Result<()> {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ApprovalClaims {
    sub: String,
    jti: String,
    exp: i64,
    op: String,
    subject_user_id: String,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    roles: Vec<String>,
}

impl ApprovalClaims {
    fn has_platform_admin_role(&self) -> bool {
        self.role.as_deref() == Some("platform_admin")
            || self.roles.iter().any(|role| role == "platform_admin")
    }
}

#[derive(Debug, Deserialize)]
struct JwtHeader {
    alg: String,
}

struct ApprovalTokenVerifier {
    verifying_key: VerifyingKey,
}

impl ApprovalTokenVerifier {
    fn from_env() -> Result<Self> {
        let raw = env::var(APPROVAL_PUBLIC_KEY_ENV)
            .or_else(|_| env::var(APPROVAL_PUBLIC_KEY_FALLBACK_ENV))
            .with_context(|| {
                format!(
                    "{APPROVAL_PUBLIC_KEY_ENV} or {APPROVAL_PUBLIC_KEY_FALLBACK_ENV} is required"
                )
            })?;
        Self::from_public_key_material(&raw)
    }

    fn from_public_key_material(raw: &str) -> Result<Self> {
        let bytes = decode_key_material(raw)?;
        let key_bytes: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("approval public key must be 32 bytes"))?;
        Ok(Self {
            verifying_key: VerifyingKey::from_bytes(&key_bytes)
                .context("invalid approval Ed25519 public key")?,
        })
    }

    fn verify(
        &self,
        token: &str,
        expected_op: &str,
        subject_user_id: &str,
        workspace: Option<&str>,
    ) -> Result<ApprovalClaims> {
        let parts = token.split('.').collect::<Vec<_>>();
        if parts.len() != 3 {
            bail!("approval token must be a compact JWT");
        }

        let header: JwtHeader = serde_json::from_slice(&decode_base64url(parts[0])?)
            .context("decoding approval token header")?;
        if header.alg != "EdDSA" {
            bail!("approval token must use EdDSA");
        }

        let claims: ApprovalClaims = serde_json::from_slice(&decode_base64url(parts[1])?)
            .context("decoding approval token claims")?;
        validate_claims(&claims, expected_op, subject_user_id, workspace)?;

        let signature_bytes = decode_base64url(parts[2])?;
        let signature_bytes: [u8; 64] = signature_bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("approval token signature must be 64 bytes"))?;
        let signature = Signature::from_bytes(&signature_bytes);
        let signed = format!("{}.{}", parts[0], parts[1]);
        self.verifying_key
            .verify(signed.as_bytes(), &signature)
            .context("approval token signature verification failed")?;
        Ok(claims)
    }
}

fn validate_claims(
    claims: &ApprovalClaims,
    expected_op: &str,
    subject_user_id: &str,
    workspace: Option<&str>,
) -> Result<()> {
    if claims.sub.trim().is_empty() {
        bail!("approval token missing sub");
    }
    if claims.jti.trim().is_empty() {
        bail!("approval token missing jti");
    }
    if claims.op != expected_op {
        bail!("approval token op must be `{expected_op}`");
    }
    if claims.subject_user_id != subject_user_id {
        bail!("approval token subject_user_id mismatch");
    }
    if !claims.has_platform_admin_role() {
        bail!("approval token requires platform_admin role");
    }
    let now = Utc::now().timestamp();
    if claims.exp <= now {
        bail!("approval token expired");
    }
    if let Some(token_workspace) = claims.workspace_id.as_deref()
        && Some(token_workspace) != workspace
    {
        bail!("approval token workspace_id mismatch");
    }
    Ok(())
}

async fn consume_approval_jti(pool: &PgPool, claims: &ApprovalClaims) -> Result<()> {
    let expires_at = Utc
        .timestamp_opt(claims.exp, 0)
        .single()
        .ok_or_else(|| anyhow!("approval token exp is out of range"))?;
    let inserted = sqlx::query_scalar::<_, String>(
        r#"
        INSERT INTO moa.audit_jti_used
            (jti, op, subject_user_id, approver_id, approval_claims, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (jti) DO NOTHING
        RETURNING jti
        "#,
    )
    .bind(&claims.jti)
    .bind(&claims.op)
    .bind(&claims.subject_user_id)
    .bind(&claims.sub)
    .bind(serde_json::to_value(claims)?)
    .bind(expires_at)
    .fetch_optional(pool)
    .await
    .context("recording approval token jti")?;
    ensure_jti_inserted(inserted.as_deref())
}

fn ensure_jti_inserted(inserted: Option<&str>) -> Result<()> {
    if inserted.is_some() {
        Ok(())
    } else {
        bail!("approval token replayed")
    }
}

struct Ed25519ManifestSigner {
    key_id: String,
    signing_key: SigningKey,
}

impl Ed25519ManifestSigner {
    fn from_env() -> Result<Self> {
        let raw = env::var(EXPORT_SIGNING_KEY_ENV)
            .or_else(|_| env::var(EXPORT_SIGNING_KEY_FALLBACK_ENV))
            .with_context(|| {
                format!("{EXPORT_SIGNING_KEY_ENV} or {EXPORT_SIGNING_KEY_FALLBACK_ENV} is required")
            })?;
        let key_id = env::var(EXPORT_SIGNING_KEY_ID_ENV)
            .unwrap_or_else(|_| "moa-privacy-export-ops".to_string());
        Self::from_signing_key_material(key_id, &raw)
    }

    fn from_signing_key_material(key_id: String, raw: &str) -> Result<Self> {
        let bytes = decode_key_material(raw)?;
        let seed = match bytes.len() {
            32 => bytes,
            64 => bytes[..32].to_vec(),
            len => bail!("export signing key must be 32 or 64 bytes, got {len}"),
        };
        let seed: [u8; 32] = seed
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("export signing key must be 32 bytes"))?;
        Ok(Self {
            key_id,
            signing_key: SigningKey::from_bytes(&seed),
        })
    }

    fn key_id(&self) -> &str {
        &self.key_id
    }

    fn public_key_hex(&self) -> String {
        hex::encode(self.signing_key.verifying_key().to_bytes())
    }

    fn sign(&self, bytes: &[u8]) -> Result<Vec<u8>> {
        Ok(self.signing_key.sign(bytes).to_bytes().to_vec())
    }
}

fn decode_base64url(value: &str) -> Result<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(value)
        .with_context(|| "invalid base64url value")
}

fn decode_key_material(raw: &str) -> Result<Vec<u8>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("key material is empty");
    }
    if trimmed.len().is_multiple_of(2)
        && trimmed
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return hex::decode(trimmed).context("invalid hex key material");
    }
    BASE64_STANDARD
        .decode(trimmed)
        .or_else(|_| URL_SAFE_NO_PAD.decode(trimmed))
        .context("key material must be hex or base64")
}

#[cfg(test)]
mod tests {
    use std::{io::Read, sync::Arc};

    use ed25519_dalek::SigningKey;
    use moa_memory_graph::{GraphStore, NodeLabel, NodeWriteIntent, PiiClass};
    use moa_memory_vector::PgvectorStore;
    use moa_session::testing;
    use serde_json::json;
    use tempfile::tempdir;
    use tokio::sync::Mutex;

    use super::*;

    static PRIVACY_ERASE_TEST_LOCK: Mutex<()> = Mutex::const_new(());

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn approval_token(claims: &ApprovalClaims, key: &SigningKey) -> String {
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"EdDSA","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("serialize claims"));
        let signed = format!("{header}.{payload}");
        let signature = key.sign(signed.as_bytes()).to_bytes();
        format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature))
    }

    fn valid_claims(subject: Uuid) -> ApprovalClaims {
        valid_claims_for(subject, "workspace-a", "export")
    }

    fn valid_claims_for(subject: Uuid, workspace: &str, op: &str) -> ApprovalClaims {
        ApprovalClaims {
            sub: "ops-admin".to_string(),
            jti: Uuid::now_v7().to_string(),
            exp: Utc::now().timestamp() + 300,
            op: op.to_string(),
            subject_user_id: subject.to_string(),
            workspace_id: Some(workspace.to_string()),
            role: None,
            roles: vec!["platform_admin".to_string()],
        }
    }

    fn basis_vector() -> Vec<f32> {
        let mut vector = vec![0.0; 1024];
        vector[0] = 1.0;
        vector
    }

    fn erase_test_graph(pool: &PgPool, workspace_id: &str, user_id: &str) -> AgeGraphStore {
        let scope = ScopeContext::user(WorkspaceId::new(workspace_id), UserId::new(user_id));
        let vector = PgvectorStore::new_for_app_role(pool.clone(), scope.clone());
        AgeGraphStore::scoped_for_app_role(pool.clone(), scope).with_vector_store(Arc::new(vector))
    }

    fn erase_test_intent(workspace_id: &str, user_id: &str, name: &str) -> NodeWriteIntent {
        NodeWriteIntent {
            uid: Uuid::now_v7(),
            label: NodeLabel::Fact,
            workspace_id: Some(workspace_id.to_string()),
            user_id: Some(user_id.to_string()),
            scope: "user".to_string(),
            name: name.to_string(),
            properties: json!({ "name": name, "user_id": user_id, "source": "privacy_erase_test" }),
            pii_class: PiiClass::Phi,
            confidence: Some(0.95),
            valid_from: Utc::now(),
            embedding: Some(basis_vector()),
            embedding_model: Some("test-model".to_string()),
            embedding_model_version: Some(1),
            actor_id: user_id.to_string(),
            actor_kind: "user".to_string(),
        }
    }

    async fn create_erase_test_node(
        pool: &PgPool,
        workspace_id: &str,
        user_id: &str,
        name: &str,
    ) -> Uuid {
        let graph = erase_test_graph(pool, workspace_id, user_id);
        let intent = erase_test_intent(workspace_id, user_id, name);
        let uid = intent.uid;
        graph
            .create_node(intent)
            .await
            .expect("create erase fixture");
        uid
    }

    async fn node_count(pool: &PgPool, uid: Uuid) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moa.node_index WHERE uid = $1")
            .bind(uid)
            .fetch_one(pool)
            .await
            .expect("count node rows")
    }

    async fn embedding_count(pool: &PgPool, uid: Uuid) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moa.embeddings WHERE uid = $1")
            .bind(uid)
            .fetch_one(pool)
            .await
            .expect("count embedding rows")
    }

    async fn erase_changelog_count(pool: &PgPool, workspace_id: &str, subject: Uuid) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM moa.graph_changelog \
             WHERE workspace_id = $1 AND op = 'erase' AND target_uid = $2",
        )
        .bind(workspace_id)
        .bind(subject)
        .fetch_one(pool)
        .await
        .expect("count erase changelog rows")
    }

    async fn total_erase_changelog_count(pool: &PgPool, workspace_id: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM moa.graph_changelog WHERE workspace_id = $1 AND op = 'erase'",
        )
        .bind(workspace_id)
        .fetch_one(pool)
        .await
        .expect("count all erase changelog rows")
    }

    #[test]
    fn privacy_export_authz_required() {
        let subject = Uuid::now_v7();
        let key = signing_key();
        let verifier = ApprovalTokenVerifier {
            verifying_key: key.verifying_key(),
        };
        let mut claims = valid_claims(subject);
        claims.roles.clear();
        let token = approval_token(&claims, &key);

        let error = verifier
            .verify(&token, "export", &subject.to_string(), Some("workspace-a"))
            .expect_err("missing platform_admin role should fail");

        assert!(error.to_string().contains("platform_admin"));
    }

    #[test]
    fn approval_token_verifies_subject_op_workspace_and_signature() {
        let subject = Uuid::now_v7();
        let key = signing_key();
        let verifier = ApprovalTokenVerifier {
            verifying_key: key.verifying_key(),
        };
        let claims = valid_claims(subject);
        let token = approval_token(&claims, &key);

        let verified = verifier
            .verify(&token, "export", &subject.to_string(), Some("workspace-a"))
            .expect("verify token");

        assert_eq!(verified.sub, "ops-admin");
        assert_eq!(verified.subject_user_id, subject.to_string());
    }

    #[test]
    fn privacy_export_jti_replay_blocked() {
        ensure_jti_inserted(Some("jti-1")).expect("first insert accepted");
        let error = ensure_jti_inserted(None).expect_err("replay should fail");
        assert!(error.to_string().contains("replayed"));
    }

    #[test]
    fn privacy_erase_authz_required() {
        let subject = Uuid::now_v7();
        let key = signing_key();
        let verifier = ApprovalTokenVerifier {
            verifying_key: key.verifying_key(),
        };
        let mut claims = valid_claims_for(subject, "workspace-a", "erase");
        claims.roles.clear();
        let token = approval_token(&claims, &key);

        let error = verifier
            .verify(&token, "erase", &subject.to_string(), Some("workspace-a"))
            .expect_err("missing platform_admin role should fail");

        assert!(error.to_string().contains("platform_admin"));
    }

    #[test]
    fn privacy_erase_jti_replay_blocked() {
        ensure_jti_inserted(Some("jti-erase")).expect("first insert accepted");
        let error = ensure_jti_inserted(None).expect_err("replay should fail");
        assert!(error.to_string().contains("replayed"));
    }

    #[tokio::test]
    async fn privacy_erase_dry_run() {
        let _guard = PRIVACY_ERASE_TEST_LOCK.lock().await;
        let (store, database_url, schema_name) = testing::create_isolated_test_store()
            .await
            .expect("create isolated test store");
        let workspace_id = format!("privacy-erase-dry-{}", Uuid::now_v7().simple());
        let subject = Uuid::now_v7();
        let uid = create_erase_test_node(
            store.pool(),
            &workspace_id,
            &subject.to_string(),
            "dry run fact",
        )
        .await;
        let before_changelog = total_erase_changelog_count(store.pool(), &workspace_id).await;
        let ctx = EraseContext {
            pool: store.pool().clone(),
            workspace_id: workspace_id.clone(),
            subject_user: subject,
            subject_user_id: subject.to_string(),
            reason: "dry run".to_string(),
            claims: valid_claims_for(subject, &workspace_id, "erase"),
        };

        let report = execute_privacy_erase(ctx, true)
            .await
            .expect("run dry erase");

        assert!(report.contains("privacy erase dry run"));
        assert!(report.contains("candidate_count: 1"));
        assert!(report.contains(&uid.to_string()));
        assert_eq!(node_count(store.pool(), uid).await, 1);
        assert_eq!(
            total_erase_changelog_count(store.pool(), &workspace_id).await,
            before_changelog
        );

        drop(store);
        testing::cleanup_test_schema(&database_url, &schema_name)
            .await
            .expect("drop isolated schema");
    }

    #[tokio::test]
    async fn privacy_erase_basic() {
        let _guard = PRIVACY_ERASE_TEST_LOCK.lock().await;
        let (store, database_url, schema_name) = testing::create_isolated_test_store()
            .await
            .expect("create isolated test store");
        let workspace_id = format!("privacy-erase-basic-{}", Uuid::now_v7().simple());
        let subject = Uuid::now_v7();
        let uid = create_erase_test_node(
            store.pool(),
            &workspace_id,
            &subject.to_string(),
            "basic erasure fact",
        )
        .await;
        assert_eq!(embedding_count(store.pool(), uid).await, 1);
        let ctx = EraseContext {
            pool: store.pool().clone(),
            workspace_id: workspace_id.clone(),
            subject_user: subject,
            subject_user_id: subject.to_string(),
            reason: "GDPR Art.17 request".to_string(),
            claims: valid_claims_for(subject, &workspace_id, "erase"),
        };

        let report = execute_privacy_erase(ctx, false)
            .await
            .expect("run erasure");

        assert!(report.contains("erased_count: 1"));
        assert_eq!(node_count(store.pool(), uid).await, 0);
        assert_eq!(embedding_count(store.pool(), uid).await, 0);
        assert_eq!(
            erase_changelog_count(store.pool(), &workspace_id, uid).await,
            1
        );
        assert_eq!(
            erase_changelog_count(store.pool(), &workspace_id, subject).await,
            1
        );

        drop(store);
        testing::cleanup_test_schema(&database_url, &schema_name)
            .await
            .expect("drop isolated schema");
    }

    #[tokio::test]
    async fn privacy_erase_idempotent() {
        let _guard = PRIVACY_ERASE_TEST_LOCK.lock().await;
        let (store, database_url, schema_name) = testing::create_isolated_test_store()
            .await
            .expect("create isolated test store");
        let workspace_id = format!("privacy-erase-idem-{}", Uuid::now_v7().simple());
        let subject = Uuid::now_v7();
        create_erase_test_node(
            store.pool(),
            &workspace_id,
            &subject.to_string(),
            "idempotent erasure fact",
        )
        .await;
        let first = EraseContext {
            pool: store.pool().clone(),
            workspace_id: workspace_id.clone(),
            subject_user: subject,
            subject_user_id: subject.to_string(),
            reason: "first erase".to_string(),
            claims: valid_claims_for(subject, &workspace_id, "erase"),
        };
        execute_privacy_erase(first, false)
            .await
            .expect("first erasure");
        let after_first = total_erase_changelog_count(store.pool(), &workspace_id).await;
        let second = EraseContext {
            pool: store.pool().clone(),
            workspace_id: workspace_id.clone(),
            subject_user: subject,
            subject_user_id: subject.to_string(),
            reason: "second erase".to_string(),
            claims: valid_claims_for(subject, &workspace_id, "erase"),
        };

        let report = execute_privacy_erase(second, false)
            .await
            .expect("second erasure");

        assert!(report.contains("candidate_count: 0"));
        assert!(report.contains("erased_count: 0"));
        assert_eq!(
            total_erase_changelog_count(store.pool(), &workspace_id).await,
            after_first
        );

        drop(store);
        testing::cleanup_test_schema(&database_url, &schema_name)
            .await
            .expect("drop isolated schema");
    }

    #[tokio::test]
    async fn privacy_erase_cross_tenant_denied() {
        let _guard = PRIVACY_ERASE_TEST_LOCK.lock().await;
        let (store, database_url, schema_name) = testing::create_isolated_test_store()
            .await
            .expect("create isolated test store");
        let workspace_a = format!("privacy-erase-a-{}", Uuid::now_v7().simple());
        let workspace_b = format!("privacy-erase-b-{}", Uuid::now_v7().simple());
        let subject = Uuid::now_v7();
        let uid_b = create_erase_test_node(
            store.pool(),
            &workspace_b,
            &subject.to_string(),
            "other workspace fact",
        )
        .await;
        let ctx = EraseContext {
            pool: store.pool().clone(),
            workspace_id: workspace_a.clone(),
            subject_user: subject,
            subject_user_id: subject.to_string(),
            reason: "wrong workspace erase".to_string(),
            claims: valid_claims_for(subject, &workspace_a, "erase"),
        };

        let report = execute_privacy_erase(ctx, false)
            .await
            .expect("wrong workspace erasure is idempotent");

        assert!(report.contains("erased_count: 0"));
        assert_eq!(node_count(store.pool(), uid_b).await, 1);
        assert_eq!(
            total_erase_changelog_count(store.pool(), &workspace_a).await,
            0
        );

        drop(store);
        testing::cleanup_test_schema(&database_url, &schema_name)
            .await
            .expect("drop isolated schema");
    }

    #[tokio::test]
    async fn privacy_erase_crypto_shred_op_rejected() {
        let _guard = PRIVACY_ERASE_TEST_LOCK.lock().await;
        let (store, database_url, schema_name) = testing::create_isolated_test_store()
            .await
            .expect("create isolated test store");
        let workspace_id = format!("privacy-erase-check-{}", Uuid::now_v7().simple());
        let mut tx = begin_app_scoped_tx(store.pool(), &workspace_id, &Uuid::now_v7().to_string())
            .await
            .expect("begin app tx");

        let error = sqlx::query(
            r#"
            INSERT INTO moa.graph_changelog
                (workspace_id, actor_id, actor_kind, op, target_kind, target_label,
                 target_uid, payload, pii_class)
            VALUES ($1, 'ops-admin', 'admin', 'crypto_shred', 'user', 'User',
                    $2, '{}'::jsonb, 'phi')
            "#,
        )
        .bind(&workspace_id)
        .bind(Uuid::now_v7())
        .execute(tx.as_mut())
        .await
        .expect_err("crypto_shred must be rejected");
        assert!(error.to_string().contains("graph_changelog_op_check"));
        tx.rollback().await.expect("rollback failed insert");

        drop(store);
        testing::cleanup_test_schema(&database_url, &schema_name)
            .await
            .expect("drop isolated schema");
    }

    #[tokio::test]
    async fn privacy_export_round_trip() {
        let subject = Uuid::now_v7();
        let dir = tempdir().expect("tempdir");
        let export_dir = dir.path().join("export");
        fs::create_dir_all(&export_dir)
            .await
            .expect("create export dir");
        fs::write(export_dir.join("facts.jsonl"), "{}\n")
            .await
            .expect("write facts");
        fs::write(export_dir.join("entities.jsonl"), "")
            .await
            .expect("write entities");
        fs::write(export_dir.join("relationships.jsonl"), "")
            .await
            .expect("write relationships");
        fs::write(export_dir.join("embeddings.jsonl"), "")
            .await
            .expect("write embeddings");
        fs::write(export_dir.join("skills.jsonl"), "")
            .await
            .expect("write skills");
        fs::write(export_dir.join("skill_addenda.jsonl"), "")
            .await
            .expect("write addenda");
        fs::write(export_dir.join("changelog.jsonl"), "")
            .await
            .expect("write changelog");

        let claims = valid_claims(subject);
        let ctx = ExportContext {
            pool: PgPool::connect_lazy("postgres://unused").expect("lazy pool"),
            workspace: Some("workspace-a".to_string()),
            subject_user: subject,
            subject_user_id: subject.to_string(),
            reason: "GDPR Art.15 request".to_string(),
            claims,
        };
        let counts = BTreeMap::from([
            ("facts", 1),
            ("entities", 0),
            ("relationships", 0),
            ("embeddings", 0),
            ("skills", 0),
            ("skill_addenda", 0),
            ("changelog", 0),
        ]);
        write_export_readme(&ctx, &counts, &export_dir)
            .await
            .expect("write readme");
        let signer = Ed25519ManifestSigner {
            key_id: "test-key".to_string(),
            signing_key: signing_key(),
        };
        write_manifest(&export_dir, &signer, &ctx, &counts)
            .await
            .expect("write manifest");

        let manifest = fs::read(export_dir.join("manifest.json"))
            .await
            .expect("read manifest");
        let signature = fs::read(export_dir.join("manifest.sig"))
            .await
            .expect("read sig");
        let signature: [u8; 64] = signature
            .as_slice()
            .try_into()
            .expect("ed25519 signature bytes");
        signer
            .signing_key
            .verifying_key()
            .verify(&manifest, &Signature::from_bytes(&signature))
            .expect("signature verifies");
        let manifest_json: Value = serde_json::from_slice(&manifest).expect("manifest json");
        assert_eq!(manifest_json["encryption"], "none");
        assert!(
            manifest_json["files"]
                .as_array()
                .expect("files")
                .iter()
                .any(|entry| entry["name"] == "facts.jsonl" && entry["sha256"].as_str().is_some())
        );

        let target = dir.path().join("subject.tgz");
        finalize_archive(&export_dir, &target, None)
            .await
            .expect("finalize archive");
        let archive = std::fs::File::open(&target).expect("open archive");
        let decoder = flate2::read::GzDecoder::new(archive);
        let mut archive = tar::Archive::new(decoder);
        let mut names = Vec::new();
        for entry in archive.entries().expect("entries") {
            let mut entry = entry.expect("entry");
            let path = entry.path().expect("entry path").display().to_string();
            let mut sink = Vec::new();
            entry.read_to_end(&mut sink).expect("read entry");
            names.push(path);
        }
        assert!(names.iter().any(|name| name == "export/manifest.json"));
        assert!(names.iter().any(|name| name == "export/manifest.sig"));
        assert!(names.iter().any(|name| name == "export/facts.jsonl"));
    }
}

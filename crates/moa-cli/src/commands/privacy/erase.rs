//! Privacy erase command implementation.

use super::*;

/// Arguments for `moa privacy erase`.
#[derive(Debug, Args)]
pub struct Args {
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

#[derive(Debug)]
pub(super) struct EraseContext {
    pub(super) pool: PgPool,
    pub(super) workspace_id: String,
    pub(super) subject_user: Uuid,
    pub(super) subject_user_id: String,
    pub(super) reason: String,
    pub(super) claims: ApprovalClaims,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub(super) struct EraseCandidate {
    uid: Uuid,
    label: String,
    name: String,
    pii_class: String,
}

pub(super) async fn run(config: &MoaConfig, args: Args) -> Result<String> {
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

pub(super) async fn execute_privacy_erase(ctx: EraseContext, dry_run: bool) -> Result<String> {
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

pub(super) async fn begin_app_scoped_tx<'a>(
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

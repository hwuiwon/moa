//! Privacy export orchestration and audit artifact helpers.

use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use chrono::Utc;
use moa_config::ComplianceConfig;
use moa_core::types::identifiers::TenantId;
use moa_memory_graph::{ChangelogRecord, write_and_bump};
use moa_wire::privacy::{PrivacyExportRequest, PrivacyExportResponse};
use restate_sdk::prelude::*;
use serde_json::{Value, json};
use sqlx::PgPool;

use super::approval::ApprovalClaims;
use super::context::{PrivacyExportContext, PrivacySubject, storage_partition_id_for_tenant};
use super::manifest::{
    Ed25519ManifestSigner, cleanup_temp_dir, create_temp_dir, finalize_archive_to_bytes,
    write_manifest,
};
use super::repository::{
    begin_privacy_export_snapshot, collect_privacy_export_data_sections, consume_approval_jti,
    resolve_privacy_export_subjects,
};
use super::{handler_error, usize_to_u64};

/// Executes a privacy export after handler-level authz and token verification.
pub async fn execute_privacy_export(
    foreground_pool: PgPool,
    background_pool: PgPool,
    tenant_id: TenantId,
    request: PrivacyExportRequest,
    claims: ApprovalClaims,
    config: ComplianceConfig,
) -> Result<PrivacyExportResponse, HandlerError> {
    if request.reason.trim().is_empty() {
        return Err(TerminalError::new_with_code(400, "--reason is required").into());
    }
    let signer = Ed25519ManifestSigner::from_config(&config)?;
    consume_approval_jti(&foreground_pool, tenant_id.0, &claims).await?;
    let storage_partition_id = storage_partition_id_for_tenant(tenant_id);
    let mut snapshot = begin_privacy_export_snapshot(&background_pool).await?;
    let resolved = resolve_privacy_export_subjects(
        snapshot.as_mut(),
        tenant_id.0,
        Some(&storage_partition_id),
        &request.subject_user_id,
    )
    .await?;
    let subject_user = resolved
        .subjects
        .first()
        .map(|subject| subject.target_uid)
        .ok_or_else(|| TerminalError::new("privacy subject resolution returned no subjects"))?;
    let storage_partition = resolved.effective_storage_partition.clone();
    let base_dir = create_temp_dir("moa-privacy-export").await?;
    let export_dir = base_dir.join("export");
    tokio::fs::create_dir_all(&export_dir)
        .await
        .map_err(handler_error)?;
    let ctx = PrivacyExportContext {
        audit_pool: foreground_pool,
        tenant_id,
        storage_partition,
        subject_user,
        subject_user_id: request.subject_user_id.to_string(),
        subjects: resolved.subjects,
        reason: request.reason,
        claims,
    };

    let result = async {
        let counts =
            collect_privacy_export_data_sections(&ctx, snapshot.as_mut(), &export_dir).await?;
        snapshot.commit().await.map_err(handler_error)?;
        write_export_readme(&ctx, &counts, &export_dir).await?;
        let manifest = write_manifest(&export_dir, &signer, &ctx, &counts).await?;
        let archive =
            finalize_archive_to_bytes(&export_dir, request.pgp_recipient.as_deref()).await?;
        emit_export_audit(&ctx, &counts).await?;
        Ok::<_, HandlerError>((counts, manifest, archive))
    }
    .await;

    cleanup_temp_dir(&base_dir).await;
    let (counts, manifest, archive) = result?;
    Ok(PrivacyExportResponse {
        subject_user_id: request.subject_user_id,
        tenant_id: ctx.tenant_id,
        archive_uri: "inline:privacy-export.tgz".to_string(),
        file_count: usize_to_u64(counts.len().saturating_add(3)),
        counts: counts
            .into_iter()
            .map(|(key, value)| (key.to_string(), usize_to_u64(value)))
            .collect(),
        manifest,
        archive_base64: Some(BASE64_STANDARD.encode(archive)),
    })
}

/// Writes the privacy export README file.
pub async fn write_export_readme(
    ctx: &PrivacyExportContext,
    counts: &BTreeMap<&'static str, usize>,
    export_dir: &Path,
) -> Result<(), HandlerError> {
    let mut lines = Vec::new();
    lines.push("# MOA subject access export".to_string());
    lines.push(String::new());
    lines.push(format!("Created at: {}", Utc::now().to_rfc3339()));
    lines.push(format!("Subject user id: {}", ctx.subject_user_id));
    lines.push(format!("Tenant: {}", ctx.tenant_id));
    lines.push("Included subjects:".to_string());
    for subject in &ctx.subjects {
        lines.push(format!(
            "- {} ({})",
            subject.user_id,
            subject.provenance.as_str()
        ));
    }
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

    tokio::fs::write(export_dir.join("README.md"), lines.join("\n"))
        .await
        .map_err(handler_error)?;
    Ok(())
}

async fn emit_export_audit(
    ctx: &PrivacyExportContext,
    counts: &BTreeMap<&'static str, usize>,
) -> Result<(), HandlerError> {
    let mut tx = ctx.audit_pool.begin().await.map_err(handler_error)?;
    let file_count = counts.len().saturating_add(4);
    write_and_bump(
        &mut tx,
        ChangelogRecord {
            storage_partition_id: ctx.storage_partition.clone(),
            contact_id: None,
            scope: "tenant".to_string(),
            actor_id: Some(ctx.claims.sub.clone()),
            actor_kind: "admin".to_string(),
            op: "export".to_string(),
            target_kind: "user".to_string(),
            target_label: "User".to_string(),
            target_uid: ctx.subject_user,
            payload: json!({
                "reason": ctx.reason,
                "subject_user_id": ctx.subject_user_id,
                "subjects": privacy_subjects_json(&ctx.subjects),
                "storage_partition": ctx.storage_partition.as_deref(),
                "artifact_counts": counts,
                "files": file_count,
            }),
            redaction_marker: None,
            pii_class: "phi".to_string(),
            audit_metadata: Some(json!({
                "approval_token_jti": ctx.claims.jti.as_str(),
                "approval_token_sub": ctx.claims.sub.as_str(),
                "subject_user_id": ctx.subject_user_id,
                "subjects": privacy_subjects_json(&ctx.subjects),
                "op": "export",
            })),
            cause_change_id: None,
        },
    )
    .await
    .map_err(handler_error)?;
    tx.commit().await.map_err(handler_error)?;
    Ok(())
}

fn privacy_subjects_json(subjects: &[PrivacySubject]) -> Value {
    Value::Array(
        subjects
            .iter()
            .map(|subject| {
                json!({
                    "user_id": subject.user_id.as_str(),
                    "target_uid": subject.target_uid,
                    "provenance": subject.provenance.as_str(),
                })
            })
            .collect(),
    )
}

//! Privacy erasure orchestration across graph memory and PII vault state.

use moa_core::wire::privacy::{ContactErasureScope, PrivacyEraseResponse};
use moa_core::{StoragePartitionId, UserId};
use moa_lineage_audit::PiiVault;
use moa_memory_pii::erasure::{
    EraseCandidate, GraphErasureAudit, enumerate_erase_candidates, hard_purge_erase_candidates,
};
use restate_sdk::prelude::*;
use serde_json::json;

use super::context::{PrivacyEraseContext, PrivacySubject};
use super::repository::{
    ContactLinkedSubjectPolicy, PrivacySubjectKind, consume_approval_jti, resolve_privacy_subjects,
};
use super::{handler_error, usize_to_u64};

const ERASE_SAMPLE_LIMIT: usize = 20;

/// Runs privacy erasure after authz and approval-token verification have completed.
pub async fn run_privacy_erase(
    ctx: PrivacyEraseContext,
) -> Result<PrivacyEraseResponse, HandlerError> {
    if ctx.reason.trim().is_empty() {
        return Err(TerminalError::new_with_code(400, "--reason is required").into());
    }
    let linked_policy = match ctx
        .contact_erasure_scope
        .unwrap_or(ContactErasureScope::SpecifiedContact)
    {
        ContactErasureScope::SpecifiedContact => ContactLinkedSubjectPolicy::SpecifiedOnly,
        ContactErasureScope::SpecifiedAndLinkedContacts => {
            ContactLinkedSubjectPolicy::IncludeVerifiedLinks
        }
    };
    let resolved = resolve_privacy_subjects(
        &ctx.pool,
        ctx.tenant_id.0,
        Some(&StoragePartitionId::new(ctx.storage_partition_id.clone())),
        &UserId::new(ctx.subject_user_id.clone()),
        linked_policy,
    )
    .await?;
    validate_contact_erasure_scope(&ctx, resolved.kind)?;
    let candidate_groups = enumerate_subject_erase_candidates(&ctx, &resolved.subjects).await?;
    let candidates = flatten_erase_candidates(&candidate_groups);

    if ctx.dry_run {
        return Ok(erase_response(&ctx, &candidates, 0, 0));
    }

    consume_approval_jti(&ctx.pool, &ctx.claims).await?;
    let pii_vault_erased = erase_pii_vault_subjects(&ctx, &resolved.subjects).await?;

    if candidates.is_empty() {
        return Ok(erase_response(&ctx, &candidates, 0, pii_vault_erased));
    }

    let mut erased_count = 0usize;
    for (subject, subject_candidates) in candidate_groups {
        if subject_candidates.is_empty() {
            continue;
        }
        erased_count = erased_count.saturating_add(
            hard_purge_erase_candidates(
                &ctx.pool,
                &graph_erasure_audit(&ctx, &subject),
                &subject_candidates,
            )
            .await
            .map_err(handler_error)?,
        );
    }

    Ok(erase_response(
        &ctx,
        &candidates,
        erased_count,
        pii_vault_erased,
    ))
}

fn validate_contact_erasure_scope(
    ctx: &PrivacyEraseContext,
    kind: PrivacySubjectKind,
) -> Result<(), HandlerError> {
    match (kind, ctx.contact_erasure_scope) {
        (PrivacySubjectKind::Contact, Some(_)) => Ok(()),
        (PrivacySubjectKind::Contact, None) => Err(TerminalError::new_with_code(
            400,
            "contact_erasure_scope is required for contact erasure",
        )
        .into()),
        (PrivacySubjectKind::User, None) => Ok(()),
        (PrivacySubjectKind::User, Some(_)) => Err(TerminalError::new_with_code(
            400,
            "contact_erasure_scope only applies to contact subjects",
        )
        .into()),
    }
}

async fn enumerate_subject_erase_candidates(
    ctx: &PrivacyEraseContext,
    subjects: &[PrivacySubject],
) -> Result<Vec<(PrivacySubject, Vec<EraseCandidate>)>, HandlerError> {
    let mut groups = Vec::with_capacity(subjects.len());
    for subject in subjects {
        let candidates = enumerate_erase_candidates(&ctx.pool, ctx.tenant_id, &subject.user_id)
            .await
            .map_err(handler_error)?;
        groups.push((subject.clone(), candidates));
    }
    Ok(groups)
}

#[derive(Debug, Clone)]
struct SubjectEraseCandidate {
    subject: PrivacySubject,
    candidate: EraseCandidate,
}

fn flatten_erase_candidates(
    groups: &[(PrivacySubject, Vec<EraseCandidate>)],
) -> Vec<SubjectEraseCandidate> {
    groups
        .iter()
        .flat_map(|(subject, candidates)| {
            candidates
                .iter()
                .cloned()
                .map(|candidate| SubjectEraseCandidate {
                    subject: subject.clone(),
                    candidate,
                })
        })
        .collect()
}

async fn erase_pii_vault_subjects(
    ctx: &PrivacyEraseContext,
    subjects: &[PrivacySubject],
) -> Result<u64, HandlerError> {
    let Some(secret) = ctx.pii_vault_secret.clone() else {
        tracing::warn!(
            storage_partition_id = %ctx.storage_partition_id,
            subject_user_id = %ctx.subject_user_id,
            "skipping PII vault erase because no PII vault secret is configured"
        );
        return Ok(0);
    };

    let vault = PiiVault::with_pool(ctx.pool.clone(), secret, "privacy-erase");
    let mut erased = 0u64;
    for subject in subjects {
        let subject_pseudonym = vault
            .subject_pseudonym(&subject.user_id)
            .map_err(handler_error)?;
        erased = erased.saturating_add(
            vault
                .erase_subject(&ctx.storage_partition_id, &subject_pseudonym)
                .await
                .map_err(handler_error)?,
        );
    }
    Ok(erased)
}

fn graph_erasure_audit(ctx: &PrivacyEraseContext, subject: &PrivacySubject) -> GraphErasureAudit {
    GraphErasureAudit {
        tenant_id: ctx.tenant_id,
        subject_user: subject.target_uid,
        subject_user_id: subject.user_id.clone(),
        reason: ctx.reason.clone(),
        approver_id: ctx.claims.sub.clone(),
        approval_token_jti: ctx.claims.jti.clone(),
    }
}

fn erase_response(
    ctx: &PrivacyEraseContext,
    candidates: &[SubjectEraseCandidate],
    erased_count: usize,
    pii_vault_erased: u64,
) -> PrivacyEraseResponse {
    PrivacyEraseResponse {
        tenant_id: ctx.tenant_id,
        subject_user_id: UserId::new(ctx.subject_user_id.clone()),
        candidate_count: usize_to_u64(candidates.len()),
        erased_count: usize_to_u64(erased_count),
        pii_vault_erased,
        dry_run: ctx.dry_run,
        sample: candidates
            .iter()
            .take(ERASE_SAMPLE_LIMIT)
            .map(|candidate| {
                json!({
                    "uid": candidate.candidate.uid,
                    "label": candidate.candidate.label,
                    "name": candidate.candidate.name,
                    "pii_class": candidate.candidate.pii_class,
                    "privacy_subject_user_id": candidate.subject.user_id.as_str(),
                    "privacy_subject_provenance": candidate.subject.provenance.as_str(),
                })
            })
            .collect(),
    }
}

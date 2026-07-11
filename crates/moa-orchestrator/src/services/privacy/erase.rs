//! Privacy erasure orchestration across graph memory and PII vault state.

use moa_core::wire::privacy::{ContactErasureScope, PrivacyEraseResponse, PrivacyEraseStatus};
use moa_core::{types::identifiers::StoragePartitionId, types::identifiers::UserId};
use moa_lineage_audit::PiiVault;
use moa_memory_pii::erasure::{
    EraseCandidate, GraphErasureAudit, delete_subject_digests, delete_subject_retrieval_lineage,
    enumerate_erase_candidates, hard_purge_erase_candidates,
};
use restate_sdk::prelude::*;
use serde_json::json;

use super::context::{PrivacyEraseContext, PrivacySubject};
use super::repository::{
    ClaimedErasureJob, ContactLinkedSubjectPolicy, ErasureJobProgress, ErasureJobStage,
    PrivacySubjectKind, claim_erasure_job, complete_erasure_job, resolve_privacy_subjects,
    save_erasure_job_progress,
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
        return Ok(dry_run_response(&ctx, &candidates));
    }

    // Bind the approval JTI to one durable, resumable job. A Restate re-execution
    // of the same request resumes from the persisted stage instead of hitting a
    // terminal "token replayed" conflict and stranding a partial erasure.
    let job = claim_erasure_job(
        &ctx.pool,
        &ctx.claims,
        &request_fingerprint(&ctx),
        ctx.tenant_id.0,
        usize_to_u64(candidates.len()),
    )
    .await?;

    // The originally enumerated candidate count is authoritative: a resumed run
    // re-enumerates only the not-yet-purged remainder.
    let candidate_count = if job.fresh {
        usize_to_u64(candidates.len())
    } else {
        job.candidate_count
    };

    if job.completed {
        return Ok(erase_response(
            &ctx,
            candidate_count,
            &candidates,
            job.progress,
        ));
    }

    let progress = run_erasure_stages(
        &ctx,
        &resolved.subjects,
        &candidate_groups,
        candidate_count,
        job,
    )
    .await?;

    complete_erasure_job(&ctx.pool, &ctx.claims.jti, &progress).await?;

    Ok(erase_response(&ctx, candidate_count, &candidates, progress))
}

/// Runs the remaining erasure stages in order, persisting progress after each so
/// a resumed job continues from where it stopped rather than restarting.
async fn run_erasure_stages(
    ctx: &PrivacyEraseContext,
    subjects: &[PrivacySubject],
    candidate_groups: &[(PrivacySubject, Vec<EraseCandidate>)],
    candidate_count: u64,
    job: ClaimedErasureJob,
) -> Result<ErasureJobProgress, HandlerError> {
    let mut progress = job.progress;

    if progress.stage == ErasureJobStage::Vault {
        progress.pii_vault_erased = erase_pii_vault_subjects(ctx, subjects).await?;
        progress.stage = ErasureJobStage::Graph;
        save_erasure_job_progress(&ctx.pool, &ctx.claims.jti, &progress).await?;
    }

    if progress.stage == ErasureJobStage::Graph {
        for (subject, subject_candidates) in candidate_groups {
            if subject_candidates.is_empty() {
                continue;
            }
            hard_purge_erase_candidates(
                &ctx.pool,
                &graph_erasure_audit(ctx, subject),
                subject_candidates,
            )
            .await
            .map_err(handler_error)?;
        }
        // After the graph stage completes without error, every enumerated
        // candidate is absent — purged in this run or an earlier partial run —
        // so the authoritative candidate count is the erased count.
        progress.graph_erased = candidate_count;
        progress.stage = ErasureJobStage::Digest;
        save_erasure_job_progress(&ctx.pool, &ctx.claims.jti, &progress).await?;
    }

    if progress.stage == ErasureJobStage::Digest {
        let mut deleted = 0u64;
        for subject in subjects {
            deleted = deleted.saturating_add(
                delete_subject_digests(&ctx.pool, ctx.tenant_id, &subject.user_id)
                    .await
                    .map_err(handler_error)?,
            );
        }
        progress.digest_deleted = deleted;
        progress.stage = ErasureJobStage::Lineage;
        save_erasure_job_progress(&ctx.pool, &ctx.claims.jti, &progress).await?;
    }

    if progress.stage == ErasureJobStage::Lineage {
        let mut deleted = 0u64;
        for subject in subjects {
            deleted = deleted.saturating_add(
                delete_subject_retrieval_lineage(&ctx.pool, ctx.tenant_id, &subject.user_id)
                    .await
                    .map_err(handler_error)?,
            );
        }
        progress.lineage_deleted = deleted;
        progress.stage = ErasureJobStage::Done;
        save_erasure_job_progress(&ctx.pool, &ctx.claims.jti, &progress).await?;
    }

    Ok(progress)
}

/// Builds a stable fingerprint of the request parameters bound to one approval
/// token, distinguishing a resume of the same request from a reuse of the token
/// for a different request.
fn request_fingerprint(ctx: &PrivacyEraseContext) -> String {
    fingerprint_parts(
        &ctx.tenant_id.to_string(),
        &ctx.subject_user_id,
        ctx.contact_erasure_scope,
        &ctx.reason,
    )
}

fn fingerprint_parts(
    tenant_id: &str,
    subject_user_id: &str,
    scope: Option<ContactErasureScope>,
    reason: &str,
) -> String {
    let scope = match scope {
        Some(ContactErasureScope::SpecifiedContact) => "specified",
        Some(ContactErasureScope::SpecifiedAndLinkedContacts) => "specified_and_linked",
        None => "none",
    };
    // The reason length precedes the reason so the delimiter can never be forged
    // by a reason that contains it.
    format!(
        "v1|tenant={tenant_id}|subject={subject_user_id}|scope={scope}|reason_len={}|reason={reason}",
        reason.len(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_fingerprint_is_stable_for_identical_requests() {
        // Pins: an identical erase request produces a byte-identical fingerprint so a
        // Restate re-execution resumes rather than being seen as a new request.
        let first = fingerprint_parts("tenant-1", "contact:abc", None, "gdpr erasure");
        let second = fingerprint_parts("tenant-1", "contact:abc", None, "gdpr erasure");
        assert_eq!(first, second);
    }

    #[test]
    fn request_fingerprint_discriminates_reason_and_scope() {
        // Pins: reusing a token for a materially different request yields a different
        // fingerprint, so the token cannot be repurposed across requests.
        let base = fingerprint_parts("tenant-1", "contact:abc", None, "gdpr erasure");
        assert_ne!(
            base,
            fingerprint_parts("tenant-1", "contact:abc", None, "different reason")
        );
        assert_ne!(
            base,
            fingerprint_parts(
                "tenant-1",
                "contact:abc",
                Some(ContactErasureScope::SpecifiedContact),
                "gdpr erasure",
            )
        );
        assert_ne!(
            base,
            fingerprint_parts("tenant-2", "contact:abc", None, "gdpr erasure")
        );
    }
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

fn dry_run_response(
    ctx: &PrivacyEraseContext,
    candidates: &[SubjectEraseCandidate],
) -> PrivacyEraseResponse {
    PrivacyEraseResponse {
        tenant_id: ctx.tenant_id,
        subject_user_id: UserId::new(ctx.subject_user_id.clone()),
        status: PrivacyEraseStatus::DryRun,
        candidate_count: usize_to_u64(candidates.len()),
        erased_count: 0,
        pii_vault_erased: 0,
        digest_deleted: 0,
        lineage_deleted: 0,
        dry_run: true,
        sample: candidate_sample(candidates),
    }
}

fn erase_response(
    ctx: &PrivacyEraseContext,
    candidate_count: u64,
    candidates: &[SubjectEraseCandidate],
    progress: ErasureJobProgress,
) -> PrivacyEraseResponse {
    PrivacyEraseResponse {
        tenant_id: ctx.tenant_id,
        subject_user_id: UserId::new(ctx.subject_user_id.clone()),
        status: PrivacyEraseStatus::Completed,
        candidate_count,
        erased_count: progress.graph_erased,
        pii_vault_erased: progress.pii_vault_erased,
        digest_deleted: progress.digest_deleted,
        lineage_deleted: progress.lineage_deleted,
        dry_run: false,
        sample: candidate_sample(candidates),
    }
}

fn candidate_sample(candidates: &[SubjectEraseCandidate]) -> Vec<serde_json::Value> {
    candidates
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
        .collect()
}

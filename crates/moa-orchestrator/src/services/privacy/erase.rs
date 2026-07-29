//! Privacy erasure orchestration across graph memory and PII vault state.

use moa_core::{
    types::identifiers::StoragePartitionId, types::identifiers::TenantId,
    types::identifiers::UserId,
};
use moa_lineage_audit::PiiVault;
use moa_memory_pii::erasure::{
    EraseCandidate, GraphErasureAudit, crypto_shred_erased_subject, delete_subject_digests,
    delete_subject_retrieval_lineage, enumerate_erase_candidates, hard_purge_erase_candidates,
};
use moa_memory_pii::learning_erasure::{
    ErasureSubjects, LearningClosure, dry_run_decisions, enumerate_learning_closure,
    erase_learning_closure, legal_hold_decisions, record_decisions,
};
use moa_wire::privacy::{ContactErasureScope, PrivacyEraseResponse, PrivacyEraseStatus};
use restate_sdk::prelude::*;
use serde_json::json;
use sqlx::PgPool;

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

    let erasure_subjects = erasure_subjects(&resolved.subjects);
    // Legal hold overrides right-to-erasure: if any included subject (or the
    // tenant) is under an active hold, refuse the erase before claiming the
    // approval JTI, taking the destruction fence, or writing anything. Failing
    // closed here keeps the erase atomic, leaves the approval token unspent so a
    // retry after release still verifies, and lets a later request resume once
    // the hold is lifted.
    //
    // A refusal is not silence. The blocked path still enumerates the
    // learning-derived closure READ-ONLY and records exactly one idempotent
    // `retained_legal_hold` decision per record, so "the hold was honored" is a
    // durable, per-record fact rather than an absence of evidence. No protected
    // byte is read or written to produce it: enumeration returns identifiers.
    if subjects_under_legal_hold(&ctx, &resolved.subjects).await? {
        let closure = enumerate_learning_closure(&ctx.pool, ctx.tenant_id, &erasure_subjects)
            .await
            .map_err(handler_error)?;
        let attempt_id =
            erasure_decision_attempt_id(&ctx.claims.jti, ErasureDecisionMode::LegalHold);
        record_decisions(
            &ctx.pool,
            ctx.tenant_id,
            &ctx.subject_user_id,
            &attempt_id,
            &legal_hold_decisions(&closure, "active legal hold blocks right-to-erasure"),
        )
        .await
        .map_err(handler_error)?;
        return Ok(blocked_by_legal_hold_response(&ctx));
    }

    let candidate_groups = enumerate_subject_erase_candidates(&ctx, &resolved.subjects).await?;
    let candidates = flatten_erase_candidates(&candidate_groups);

    if ctx.dry_run {
        // A dry run persists a typed PLANNED disposition per record with
        // `applied = false`. It must never persist a deletion it did not perform:
        // a false record of destruction is worse than no record at all, because
        // it would be read later as proof the subject's data is gone.
        let closure = enumerate_learning_closure(&ctx.pool, ctx.tenant_id, &erasure_subjects)
            .await
            .map_err(handler_error)?;
        let attempt_id = erasure_decision_attempt_id(&ctx.claims.jti, ErasureDecisionMode::DryRun);
        record_decisions(
            &ctx.pool,
            ctx.tenant_id,
            &ctx.subject_user_id,
            &attempt_id,
            &dry_run_decisions(&closure),
        )
        .await
        .map_err(handler_error)?;
        return Ok(dry_run_response(&ctx, &candidates));
    }

    // Four-eyes gate: when the tenant policy requires dual control for erasure,
    // consume a distinct second-admin approval bound to this exact request before
    // any destructive work. Fails closed (403) when no valid approval exists, so
    // nothing is purged or crypto-shredded without it. A no-op when the policy is
    // off, preserving the single-admin erasure behavior. Placed after the dry-run
    // short-circuit so a dry run never consumes an approval, and before
    // `claim_erasure_job` so the destructive path is never entered unapproved.
    ensure_erase_dual_control(&ctx).await?;

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

    let destruction_subjects = resolved
        .subjects
        .iter()
        .map(|subject| subject.target_uid)
        .collect::<Vec<_>>();
    let destruction_operation_id = destruction_operation_id(ctx.tenant_id, &destruction_subjects);
    match moa_memory_pii::legal_hold::start_destruction(
        &ctx.pool,
        ctx.tenant_id,
        &destruction_subjects,
        &destruction_operation_id,
        DUAL_CONTROL_OPERATION_ERASE,
    )
    .await
    {
        Ok(()) => {}
        Err(moa_memory_pii::legal_hold::LegalHoldError::ActiveHold) => {
            return Ok(blocked_by_legal_hold_response(&ctx));
        }
        Err(error) => return Err(handler_error(error)),
    }

    if job.completed {
        // `complete_erasure_job` and the fence completion are separate durable
        // commits. A crash between them resumes here, so close that gap before
        // reporting success instead of leaving an in-progress fence forever.
        moa_memory_pii::legal_hold::complete_destruction(
            &ctx.pool,
            ctx.tenant_id,
            &destruction_subjects,
            &destruction_operation_id,
        )
        .await
        .map_err(handler_error)?;
        return Ok(erase_response(
            &ctx,
            candidate_count,
            &candidates,
            job.progress,
        ));
    }

    // Enumerated AFTER the destruction fence is committed, so the closure is a
    // stable snapshot rather than a set that concurrent learning can grow between
    // enumeration and deletion. The fence also refuses new contribution inserts
    // for the tenant while it is in progress, which is the other half of the same
    // guarantee: without it, a turn completing mid-erase could file derived
    // learning that survives the run.
    let closure = enumerate_learning_closure(&ctx.pool, ctx.tenant_id, &erasure_subjects)
        .await
        .map_err(handler_error)?;

    let applied_attempt_id =
        erasure_decision_attempt_id(&ctx.claims.jti, ErasureDecisionMode::Applied);
    let progress = run_erasure_stages(
        &ctx,
        &resolved.subjects,
        &candidate_groups,
        candidate_count,
        job,
        &closure,
        ErasureAttempt {
            subject_user_id: &ctx.subject_user_id,
            attempt_id: &applied_attempt_id,
            destruction_operation_id: &destruction_operation_id,
        },
    )
    .await?;

    complete_erasure_job(&ctx.pool, &ctx.claims.jti, &progress).await?;
    moa_memory_pii::legal_hold::complete_destruction(
        &ctx.pool,
        ctx.tenant_id,
        &destruction_subjects,
        &destruction_operation_id,
    )
    .await
    .map_err(handler_error)?;

    Ok(erase_response(&ctx, candidate_count, &candidates, progress))
}

/// Runs the remaining erasure stages in order, persisting progress after each so
/// a resumed job continues from where it stopped rather than restarting.
/// The identity of one erasure attempt, as the decision ledger records it.
///
/// Groups the subject-scoped ledger identity with the destruction fence so the
/// similarly shaped strings cannot be transposed at a call site.
struct ErasureAttempt<'a> {
    /// Subject whose export may read this attempt's decisions.
    subject_user_id: &'a str,
    /// Identity of this one execution.
    attempt_id: &'a str,
    /// Destruction fence this attempt runs under, held for each stage guard.
    destruction_operation_id: &'a str,
}

async fn run_erasure_stages(
    ctx: &PrivacyEraseContext,
    subjects: &[PrivacySubject],
    candidate_groups: &[(PrivacySubject, Vec<EraseCandidate>)],
    candidate_count: u64,
    job: ClaimedErasureJob,
    closure: &LearningClosure,
    attempt: ErasureAttempt<'_>,
) -> Result<ErasureJobProgress, HandlerError> {
    let ErasureAttempt {
        subject_user_id,
        attempt_id,
        destruction_operation_id,
    } = attempt;
    let mut progress = job.progress;
    let destruction_subjects = subjects
        .iter()
        .map(|subject| subject.target_uid)
        .collect::<Vec<_>>();

    // Reverse-derived erasure runs FIRST, while the memories the learning points
    // at still exist. Running it after the graph stage would leave the closure
    // walk nothing to walk: the sources would already be gone and the derived
    // learning would survive unattributably, which is the exact failure this
    // stage exists to prevent.
    if progress.stage == ErasureJobStage::Learning {
        let guard = moa_memory_pii::legal_hold::begin_destruction_stage_guard(
            &ctx.pool,
            ctx.tenant_id,
            &destruction_subjects,
            destruction_operation_id,
        )
        .await
        .map_err(handler_error)?;
        // The deletions and the dispositions that explain them commit in ONE
        // transaction inside this call. Splitting them would leave a window in
        // which rows are gone and nothing on record says why or under whose
        // authority — the one state a subject-access request cannot be answered
        // from.
        let decisions = erase_learning_closure(
            &ctx.pool,
            ctx.tenant_id,
            subject_user_id,
            attempt_id,
            closure,
        )
        .await
        .map_err(handler_error)?;
        guard.finish().await.map_err(handler_error)?;
        progress.learning_erased = usize_to_u64(closure.candidate_ids.len())
            .saturating_add(usize_to_u64(closure.learning_ids.len()));
        progress.artifact_erased = usize_to_u64(closure.revision_uids.len())
            .saturating_add(usize_to_u64(closure.suite_contribution_uids.len()));
        progress.decisions_recorded = usize_to_u64(decisions.len());
        progress.stage = ErasureJobStage::Artifacts;
        save_erasure_job_progress(&ctx.pool, &ctx.claims.jti, &progress).await?;
    }

    if progress.stage == ErasureJobStage::Artifacts {
        // Nothing to re-record: the Learning stage committed its dispositions
        // atomically with its deletions, so reaching this stage already means the
        // ledger is complete for that closure. This stage exists as the resume
        // point between the learning-derived work and the vault/graph stages.
        progress.stage = ErasureJobStage::Vault;
        save_erasure_job_progress(&ctx.pool, &ctx.claims.jti, &progress).await?;
    }

    if progress.stage == ErasureJobStage::Vault {
        let guard = moa_memory_pii::legal_hold::begin_destruction_stage_guard(
            &ctx.pool,
            ctx.tenant_id,
            &destruction_subjects,
            destruction_operation_id,
        )
        .await
        .map_err(handler_error)?;
        progress.pii_vault_erased = erase_pii_vault_subjects(ctx, subjects).await?;
        guard.finish().await.map_err(handler_error)?;
        progress.stage = ErasureJobStage::Graph;
        save_erasure_job_progress(&ctx.pool, &ctx.claims.jti, &progress).await?;
    }

    if progress.stage == ErasureJobStage::Graph {
        let graph_guard = moa_memory_pii::legal_hold::begin_destruction_stage_guard(
            &ctx.pool,
            ctx.tenant_id,
            &destruction_subjects,
            destruction_operation_id,
        )
        .await
        .map_err(handler_error)?;
        for (subject, subject_candidates) in candidate_groups {
            hard_purge_erase_candidates(
                &ctx.pool,
                &graph_erasure_audit(ctx, subject),
                subject_candidates,
            )
            .await
            .map_err(handler_error)?;
        }
        graph_guard.finish().await.map_err(handler_error)?;
        // Defense-in-depth: after hard-purging the live rows, destroy each
        // subject's per-subject KEK so any sealed restricted/PHI content is
        // cryptographically unrecoverable even where a `DELETE` cannot reach
        // (backups, read replicas, WAL). Idempotent, so a resumed Graph stage
        // re-shreds harmlessly; every subject is shredded even when it had no
        // enumerated live candidates.
        //
        // `subject.target_uid` is the write-time `subject_id` (a contact's
        // UUID), matching how the graph write path keyed the KEK. KMS is a
        // required context dependency: erasure fails instead of silently
        // skipping crypto-shred when composition is incomplete.
        for subject in subjects {
            crypto_shred_erased_subject(
                &ctx.pool,
                ctx.kms.as_ref(),
                ctx.tenant_id,
                subject.target_uid,
                destruction_operation_id,
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
        let guard = moa_memory_pii::legal_hold::begin_destruction_stage_guard(
            &ctx.pool,
            ctx.tenant_id,
            &destruction_subjects,
            destruction_operation_id,
        )
        .await
        .map_err(handler_error)?;
        let mut deleted = 0u64;
        for subject in subjects {
            deleted = deleted.saturating_add(
                delete_subject_digests(&ctx.pool, ctx.tenant_id, &subject.user_id)
                    .await
                    .map_err(handler_error)?,
            );
        }
        guard.finish().await.map_err(handler_error)?;
        progress.digest_deleted = deleted;
        progress.stage = ErasureJobStage::Lineage;
        save_erasure_job_progress(&ctx.pool, &ctx.claims.jti, &progress).await?;
    }

    if progress.stage == ErasureJobStage::Lineage {
        let guard = moa_memory_pii::legal_hold::begin_destruction_stage_guard(
            &ctx.pool,
            ctx.tenant_id,
            &destruction_subjects,
            destruction_operation_id,
        )
        .await
        .map_err(handler_error)?;
        let mut deleted = 0u64;
        for subject in subjects {
            deleted = deleted.saturating_add(
                delete_subject_retrieval_lineage(&ctx.pool, ctx.tenant_id, &subject.user_id)
                    .await
                    .map_err(handler_error)?,
            );
        }
        guard.finish().await.map_err(handler_error)?;
        progress.lineage_deleted = deleted;
        progress.stage = ErasureJobStage::Done;
        save_erasure_job_progress(&ctx.pool, &ctx.claims.jti, &progress).await?;
    }

    Ok(progress)
}

fn destruction_operation_id(tenant_id: TenantId, subjects: &[uuid::Uuid]) -> String {
    let mut subjects = subjects.to_vec();
    subjects.sort_unstable();
    subjects.dedup();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"moa.privacy.subject-erasure-fence.v1");
    hasher.update(tenant_id.0.as_bytes());
    for subject in subjects {
        hasher.update(subject.as_bytes());
    }
    format!("v1:blake3:{}", hasher.finalize().to_hex())
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

/// Builds the ledger identity for one outcome mode under an approval token.
///
/// Dry runs and legal-hold refusals deliberately do not consume the JTI, so a
/// later applied run may reuse it. Including the mode keeps each replay
/// idempotent without letting an earlier unapplied row mask that applied run.
#[derive(Clone, Copy)]
enum ErasureDecisionMode {
    LegalHold,
    DryRun,
    Applied,
}

impl ErasureDecisionMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::LegalHold => "legal_hold",
            Self::DryRun => "dry_run",
            Self::Applied => "applied",
        }
    }
}

fn erasure_decision_attempt_id(approval_jti: &str, mode: ErasureDecisionMode) -> String {
    format!("{approval_jti}:{}", mode.as_str())
}

/// Dual-control operation type identifying a privacy erasure.
pub const DUAL_CONTROL_OPERATION_ERASE: &str = "privacy.erase";

/// Builds the canonical dual-control operation reference for one erasure request.
///
/// The first admin's `request` and the erase execute path derive this from the
/// same parameters (tenant, subject, scope, reason) so a second admin's approval
/// binds to exactly one erasure and cannot be redeemed for a different one. It
/// reuses the erasure request fingerprint, so the dual-control approval is keyed
/// to the same request identity as the durable erasure job.
#[must_use]
pub fn erase_operation_ref(
    tenant_id: TenantId,
    subject_user_id: &str,
    contact_erasure_scope: Option<ContactErasureScope>,
    reason: &str,
) -> String {
    fingerprint_parts(
        &tenant_id.to_string(),
        subject_user_id,
        contact_erasure_scope,
        reason,
    )
}

/// Enforces the four-eyes dual-control gate for one erasure when the tenant policy
/// requires it, consuming a distinct second-admin approval bound to this request.
///
/// Returns `Ok(())` immediately when dual control is not required (single-admin
/// erasure is unchanged). When required, it consumes an approval keyed to the
/// erasure request fingerprint, using the approval-token JTI as the idempotency
/// key so a durable re-execution of this same erasure is admitted without a second
/// approval. Fails closed (403) when no valid, distinct approval exists.
pub async fn ensure_erase_dual_control(ctx: &PrivacyEraseContext) -> Result<(), HandlerError> {
    if !ctx.require_dual_control {
        return Ok(());
    }
    let operation_ref = erase_operation_ref(
        ctx.tenant_id,
        &ctx.subject_user_id,
        ctx.contact_erasure_scope,
        &ctx.reason,
    );
    consume_erase_approval(&ctx.pool, ctx.tenant_id, &operation_ref, &ctx.claims.jti).await
}

async fn consume_erase_approval(
    pool: &PgPool,
    tenant_id: TenantId,
    operation_ref: &str,
    consumer_ref: &str,
) -> Result<(), HandlerError> {
    crate::services::dual_control::consume_approval_for(
        pool,
        tenant_id,
        DUAL_CONTROL_OPERATION_ERASE,
        operation_ref,
        consumer_ref,
    )
    .await
    .map_err(|error| {
        tracing::warn!(
            tenant_id = %tenant_id,
            operation_type = DUAL_CONTROL_OPERATION_ERASE,
            "privacy erase refused: dual-control approval unavailable"
        );
        error.into_handler_error()
    })
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

    #[test]
    fn decision_attempt_identity_distinguishes_unapplied_and_applied_runs() {
        // Pins: reusing an unconsumed approval after a dry run or legal hold
        // records a later applied attempt instead of conflicting with the plan.
        let jti = "approval-jti";
        assert_eq!(
            erasure_decision_attempt_id(jti, ErasureDecisionMode::DryRun),
            "approval-jti:dry_run"
        );
        assert_ne!(
            erasure_decision_attempt_id(jti, ErasureDecisionMode::DryRun),
            erasure_decision_attempt_id(jti, ErasureDecisionMode::Applied)
        );
        assert_ne!(
            erasure_decision_attempt_id(jti, ErasureDecisionMode::LegalHold),
            erasure_decision_attempt_id(jti, ErasureDecisionMode::Applied)
        );
    }
}

/// Returns true when any included subject is under an active legal hold.
///
/// Checks every resolved subject (the primary subject and any linked contacts)
/// because the erase is one atomic operation: a hold on any of them must block
/// the whole request rather than partially erase the unheld ones.
async fn subjects_under_legal_hold(
    ctx: &PrivacyEraseContext,
    subjects: &[PrivacySubject],
) -> Result<bool, HandlerError> {
    for subject in subjects {
        if moa_memory_pii::legal_hold::active_hold_for(&ctx.pool, ctx.tenant_id, subject.target_uid)
            .await
            .map_err(handler_error)?
        {
            tracing::warn!(
                tenant_id = %ctx.tenant_id,
                subject_provenance = subject.provenance.as_str(),
                "privacy erase blocked by active legal hold"
            );
            return Ok(true);
        }
    }
    Ok(false)
}

/// Builds the terminal response returned when a legal hold blocks the erase.
///
/// Reports zero erased counts and the [`PrivacyEraseStatus::BlockedByLegalHold`]
/// status so a caller sees the refusal explicitly rather than a silent success.
fn blocked_by_legal_hold_response(ctx: &PrivacyEraseContext) -> PrivacyEraseResponse {
    PrivacyEraseResponse {
        tenant_id: ctx.tenant_id,
        subject_user_id: UserId::new(ctx.subject_user_id.clone()),
        status: PrivacyEraseStatus::BlockedByLegalHold,
        candidate_count: 0,
        erased_count: 0,
        pii_vault_erased: 0,
        digest_deleted: 0,
        lineage_deleted: 0,
        dry_run: ctx.dry_run,
        sample: Vec::new(),
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

/// Projects resolved privacy subjects into the two identifier forms the
/// normalized learning schema keys on.
fn erasure_subjects(subjects: &[PrivacySubject]) -> ErasureSubjects {
    ErasureSubjects {
        user_ids: subjects
            .iter()
            .map(|subject| subject.user_id.to_string())
            .collect(),
        contact_ids: subjects.iter().map(|subject| subject.target_uid).collect(),
    }
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

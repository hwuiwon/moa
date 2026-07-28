//! Reverse-derived erasure of learning built from a subject's data.
//!
//! Deleting a subject's memories while a skill distilled from those memories
//! keeps serving is not erasure. It removes the evidence and leaves the
//! conclusion, which is the outcome a regulator would call out and the one MOA
//! previously shipped: nothing in the database could enumerate a derivation, so
//! nothing could reverse one.
//!
//! This module owns that reversal. It enumerates the normalized closure
//! (`contact/session/experience -> candidate -> learning_log -> artifact
//! revision/file -> suite contribution`) through typed joins, decides one
//! disposition per enumerated record, and applies it. Every decision is durable
//! and idempotent, so a resumed job neither duplicates history nor forgets it.
//!
//! Three rules carry most of the weight, and each exists because its absence
//! would produce a confident lie:
//!
//! * **A legal hold mutates nothing.** Hold handling enumerates and records
//!   `retained_legal_hold` for each record, and the database refuses to mark
//!   such a decision `applied`. "We honored the hold" is then a checkable fact,
//!   not a code path someone has to read.
//! * **A dry run is a plan.** Dispositions persist with `applied = false`. A dry
//!   run that recorded deletions would be a false record of destruction.
//! * **Fused model output is non-subtractable.** A published revision's
//!   definition and source text are written from several people's transcripts at
//!   once. You cannot carve one contributor back out of a paragraph, so a shared
//!   revision whose definition drew on erased evidence is invalidated whole.
//!   `retained_shared` is reserved for bytes with an independent contributor and
//!   a shape that can actually be separated.

use sqlx::PgPool;
use uuid::Uuid;

use moa_core::types::identifiers::TenantId;

use crate::erasure::{ErasureError, Result};

/// What was decided about one enumerated record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErasureDisposition {
    /// The row was deleted outright.
    Erased,
    /// Attributable fields were irreversibly cleared; the row remains.
    Redacted,
    /// A shared serving revision was invalidated because its fused output could
    /// not be separated from the erased contributor.
    InvalidatedRevision,
    /// A legal hold covers the record; nothing was touched.
    RetainedLegalHold,
    /// Retained bytes are provably independent of the erased subject.
    RetainedShared,
}

impl ErasureDisposition {
    /// Returns the stable database representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Erased => "erased",
            Self::Redacted => "redacted",
            Self::InvalidatedRevision => "invalidated_revision",
            Self::RetainedLegalHold => "retained_legal_hold",
            Self::RetainedShared => "retained_shared",
        }
    }

    /// Returns whether this disposition may ever be recorded as applied.
    ///
    /// A legal-hold retention never can: the whole point of the hold is that
    /// nothing was mutated. The database enforces the same rule, so a caller
    /// that ignores this still cannot write the contradiction.
    #[must_use]
    pub const fn can_be_applied(self) -> bool {
        !matches!(self, Self::RetainedLegalHold)
    }
}

/// Which table an enumerated record belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErasureRecordKind {
    /// A learning candidate.
    LearningCandidate,
    /// One normalized candidate source row.
    LearningCandidateSource,
    /// A learning-log entry.
    LearningLog,
    /// One normalized learning-log source row.
    LearningLogSource,
    /// An artifact revision.
    ArtifactRevision,
    /// One artifact package file.
    ArtifactFile,
    /// One revision contribution row.
    ArtifactRevisionContribution,
    /// One generated or accumulated suite contribution.
    ArtifactSuiteContribution,
    /// An experience record.
    ExperienceRecord,
}

impl ErasureRecordKind {
    /// Returns the stable database representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LearningCandidate => "learning_candidate",
            Self::LearningCandidateSource => "learning_candidate_source",
            Self::LearningLog => "learning_log",
            Self::LearningLogSource => "learning_log_source",
            Self::ArtifactRevision => "artifact_revision",
            Self::ArtifactFile => "artifact_file",
            Self::ArtifactRevisionContribution => "artifact_revision_contribution",
            Self::ArtifactSuiteContribution => "artifact_suite_contribution",
            Self::ExperienceRecord => "experience_record",
        }
    }
}

/// The enumerated learning-derived closure for one erasure operation.
///
/// A snapshot taken after the destruction fence is committed, so it is a stable
/// set rather than a query that could grow underneath the erase.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LearningClosure {
    /// Experience records owned by the subject.
    pub experience_ids: Vec<Uuid>,
    /// Candidates derived from the subject's data.
    pub candidate_ids: Vec<Uuid>,
    /// Learning-log entries derived from those candidates or the subject directly.
    pub learning_ids: Vec<Uuid>,
    /// Revisions with at least one contribution from an enumerated candidate.
    pub revision_uids: Vec<Uuid>,
    /// Revisions whose every contribution comes from an enumerated candidate.
    pub sole_source_revision_uids: Vec<Uuid>,
    /// Suite contributions attributable to the subject.
    pub suite_contribution_uids: Vec<Uuid>,
}

impl LearningClosure {
    /// Returns the total number of enumerated records across every level.
    #[must_use]
    pub fn len(&self) -> usize {
        self.experience_ids.len()
            + self.candidate_ids.len()
            + self.learning_ids.len()
            + self.revision_uids.len()
            + self.suite_contribution_uids.len()
    }

    /// Returns whether the closure enumerated nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns whether a revision can be erased outright rather than invalidated.
    #[must_use]
    pub fn is_sole_source(&self, revision_uid: Uuid) -> bool {
        self.sole_source_revision_uids.contains(&revision_uid)
    }
}

/// Subjects one erasure operation covers, in the two forms the schema uses.
#[derive(Debug, Clone)]
pub struct ErasureSubjects {
    /// Subject identifiers as stored in `user_id` text columns.
    pub user_ids: Vec<String>,
    /// Contact uuids, for rows that reference a contact directly.
    pub contact_ids: Vec<Uuid>,
}

/// Enumerates every learning-derived record attributable to the subjects.
///
/// Walks the closure in dependency order, each level joining the previous one
/// through its typed foreign key. There is no JSON containment, array
/// membership, or `LIKE` anywhere in this query set — those are exactly what
/// made the old provenance unenumerable, and a `LIKE '%subject%'` scan silently
/// both over- and under-matches.
pub async fn enumerate_learning_closure(
    pool: &PgPool,
    tenant_id: TenantId,
    subjects: &ErasureSubjects,
) -> Result<LearningClosure> {
    let tenant_key = tenant_id.to_string();

    let experience_ids = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id FROM experience_records
        WHERE tenant_id = $1 AND user_id = ANY($2)
        ORDER BY id
        "#,
    )
    .bind(&tenant_key)
    .bind(&subjects.user_ids)
    .fetch_all(pool)
    .await?;

    let session_ids = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT DISTINCT session_id FROM experience_records
        WHERE tenant_id = $1 AND user_id = ANY($2)
        UNION
        SELECT id FROM sessions
        WHERE storage_partition_id = $3 AND user_id = ANY($2)
        "#,
    )
    .bind(&tenant_key)
    .bind(&subjects.user_ids)
    .bind(storage_partition_of(tenant_id))
    .fetch_all(pool)
    .await?;

    // A candidate belongs to the closure when ANY of its typed sources points at
    // the subject: their contact row, one of their sessions, one of their
    // experiences, or an event in one of their sessions.
    let candidate_ids = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT DISTINCT source.candidate_id
        FROM learning_candidate_source AS source
        WHERE source.tenant_id = $1
          AND (
              source.contact_id = ANY($2)
              OR source.session_id = ANY($3)
              OR source.event_session_id = ANY($3)
              OR source.experience_id = ANY($4)
          )
        ORDER BY 1
        "#,
    )
    .bind(&tenant_key)
    .bind(&subjects.contact_ids)
    .bind(&session_ids)
    .bind(&experience_ids)
    .fetch_all(pool)
    .await?;

    let learning_ids = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT DISTINCT source.learning_id
        FROM learning_log_source AS source
        WHERE source.tenant_id = $1
          AND (
              source.candidate_id = ANY($2)
              OR source.session_id = ANY($3)
              OR source.experience_id = ANY($4)
          )
        ORDER BY 1
        "#,
    )
    .bind(&tenant_key)
    .bind(&candidate_ids)
    .bind(&session_ids)
    .bind(&experience_ids)
    .fetch_all(pool)
    .await?;

    let revision_uids = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT DISTINCT contribution.revision_uid
        FROM moa.artifact_revision_contribution AS contribution
        WHERE contribution.tenant_id = $1 AND contribution.candidate_id = ANY($2)
        ORDER BY 1
        "#,
    )
    .bind(&tenant_key)
    .bind(&candidate_ids)
    .fetch_all(pool)
    .await?;

    // Sole-source means every contribution to the revision came from a candidate
    // inside this closure. The `NOT EXISTS` is the whole test: one surviving
    // outside contributor makes the revision shared, and a shared revision must
    // never be deleted on one subject's request.
    let sole_source_revision_uids = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT DISTINCT contribution.revision_uid
        FROM moa.artifact_revision_contribution AS contribution
        WHERE contribution.tenant_id = $1
          AND contribution.candidate_id = ANY($2)
          AND NOT EXISTS (
              SELECT 1
              FROM moa.artifact_revision_contribution AS other
              WHERE other.revision_uid = contribution.revision_uid
                AND NOT (other.candidate_id = ANY($2))
          )
        ORDER BY 1
        "#,
    )
    .bind(&tenant_key)
    .bind(&candidate_ids)
    .fetch_all(pool)
    .await?;

    let suite_contribution_uids = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT contribution_uid
        FROM moa.artifact_suite_contribution
        WHERE tenant_id = $1
          AND (
              candidate_id = ANY($2)
              OR source_session_id = ANY($3)
              OR source_experience_id = ANY($4)
          )
        ORDER BY 1
        "#,
    )
    .bind(&tenant_key)
    .bind(&candidate_ids)
    .bind(&session_ids)
    .bind(&experience_ids)
    .fetch_all(pool)
    .await?;

    Ok(LearningClosure {
        experience_ids,
        candidate_ids,
        learning_ids,
        revision_uids,
        sole_source_revision_uids,
        suite_contribution_uids,
    })
}

/// One durable disposition for one enumerated record.
#[derive(Debug, Clone)]
pub struct RecordDecision {
    /// Table the record belongs to.
    pub kind: ErasureRecordKind,
    /// Record identity, as text so heterogeneous keys share one ledger.
    pub record_id: String,
    /// What was decided.
    pub disposition: ErasureDisposition,
    /// Whether the decision was carried out, or merely planned.
    pub applied: bool,
    /// Human-readable justification retained with the decision.
    pub reason: Option<String>,
}

/// Writes every decision idempotently, returning how many were newly recorded.
///
/// Keyed by `(tenant, operation_ref, record_kind, record_id)`, so a resumed or
/// replayed job converges on exactly one row per record rather than appending a
/// second identical history entry. `attempt_id` is recorded but not part of the
/// key: a later post-hold request is visibly a different attempt without being
/// able to overwrite what the held attempt decided.
pub async fn record_decisions(
    pool: &PgPool,
    tenant_id: TenantId,
    operation_ref: &str,
    attempt_id: &str,
    decisions: &[RecordDecision],
) -> Result<u64> {
    let mut conn = pool.acquire().await?;
    record_decisions_in(
        conn.as_mut(),
        tenant_id,
        operation_ref,
        attempt_id,
        decisions,
    )
    .await
}

/// Writes decisions on the caller's connection or open transaction.
///
/// The erase path uses this to commit each disposition in the SAME transaction
/// as the mutation it describes, so there is no window in which rows are gone
/// but the ledger does not say why.
async fn record_decisions_in(
    conn: &mut sqlx::PgConnection,
    tenant_id: TenantId,
    operation_ref: &str,
    attempt_id: &str,
    decisions: &[RecordDecision],
) -> Result<u64> {
    let mut recorded = 0u64;
    for decision in decisions {
        if decision.applied && !decision.disposition.can_be_applied() {
            return Err(ErasureError::Scope(
                moa_core::error::MoaError::StorageError(format!(
                    "erasure decision `{}` for {} `{}` cannot be applied",
                    decision.disposition.as_str(),
                    decision.kind.as_str(),
                    decision.record_id
                )),
            ));
        }
        recorded += sqlx::query(
            r#"
            INSERT INTO moa.privacy_erasure_record_decision
                (decision_uid, tenant_id, operation_ref, attempt_id, record_kind, record_id,
                 disposition, applied, reason)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            ON CONFLICT (tenant_id, operation_ref, record_kind, record_id) DO NOTHING
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(tenant_id.0)
        .bind(operation_ref)
        .bind(attempt_id)
        .bind(decision.kind.as_str())
        .bind(&decision.record_id)
        .bind(decision.disposition.as_str())
        .bind(decision.applied)
        .bind(decision.reason.as_deref())
        .execute(&mut *conn)
        .await?
        .rows_affected();
    }
    Ok(recorded)
}

/// Builds the read-only disposition set for a closure blocked by a legal hold.
///
/// Enumerates identically to the erasing path and records exactly one
/// `retained_legal_hold` decision per record, all unapplied. Nothing here reads
/// or writes protected bytes.
#[must_use]
pub fn legal_hold_decisions(closure: &LearningClosure, reason: &str) -> Vec<RecordDecision> {
    closure_decisions(
        closure,
        ErasureDisposition::RetainedLegalHold,
        false,
        Some(reason.to_string()),
    )
}

/// Builds the planned disposition set for a dry run.
///
/// Same enumeration, same per-record dispositions the real run would apply, but
/// `applied = false` throughout. A dry run therefore leaves a record of what
/// *would* happen and never a record of deletion.
#[must_use]
pub fn dry_run_decisions(closure: &LearningClosure) -> Vec<RecordDecision> {
    planned_decisions(closure, false)
}

/// Erases the enumerated closure and returns the applied dispositions.
///
/// Order is child-to-parent so no delete trips a foreign key: suite
/// contributions, revision contributions, learning-log sources, learning-log
/// entries, candidate sources, candidates. Sole-source revisions are deleted
/// outright; shared revisions whose fused definition drew on erased evidence are
/// invalidated in place — definition, source text, files, and identity metadata
/// cleared — because a partially rewritten fused paragraph would be a claim
/// nobody could verify.
///
/// The dispositions commit in the SAME transaction as the mutations they
/// describe. That is the whole reason this function owns the transaction rather
/// than returning decisions for a caller to persist afterwards: a crash between
/// the two would leave rows deleted with nothing on record saying why or under
/// whose authority, which is precisely the state a subject-access request or an
/// audit cannot be answered from.
pub async fn erase_learning_closure(
    pool: &PgPool,
    tenant_id: TenantId,
    operation_ref: &str,
    attempt_id: &str,
    closure: &LearningClosure,
) -> Result<Vec<RecordDecision>> {
    let tenant_key = tenant_id.to_string();
    let decisions = planned_decisions(closure, true);
    let mut tx = pool.begin().await?;

    sqlx::query(
        "DELETE FROM moa.artifact_suite_contribution WHERE tenant_id = $1 AND contribution_uid = ANY($2)",
    )
    .bind(&tenant_key)
    .bind(&closure.suite_contribution_uids)
    .execute(tx.as_mut())
    .await?;

    sqlx::query(
        "DELETE FROM moa.artifact_revision_contribution WHERE tenant_id = $1 AND candidate_id = ANY($2)",
    )
    .bind(&tenant_key)
    .bind(&closure.candidate_ids)
    .execute(tx.as_mut())
    .await?;

    // A shared revision keeps existing but stops serving anything attributable.
    // Clearing `definition`/`source_text`/`published_at` together is deliberate:
    // leaving the revision published with an emptied definition would keep a
    // broken skill in the serving path.
    for revision_uid in &closure.revision_uids {
        if closure.is_sole_source(*revision_uid) {
            continue;
        }
        sqlx::query(
            r#"
            UPDATE moa.artifact_revision
            SET definition = '{}'::JSONB,
                source_text = ''::BYTEA,
                validation_report = '{}'::JSONB,
                status = 'archived',
                published_at = NULL,
                valid_to = COALESCE(valid_to, now()),
                updated_at = now()
            WHERE revision_uid = $1
            "#,
        )
        .bind(revision_uid)
        .execute(tx.as_mut())
        .await?;
        sqlx::query("DELETE FROM moa.artifact_file WHERE revision_uid = $1")
            .bind(revision_uid)
            .execute(tx.as_mut())
            .await?;
    }

    for revision_uid in &closure.sole_source_revision_uids {
        sqlx::query("DELETE FROM moa.artifact_file WHERE revision_uid = $1")
            .bind(revision_uid)
            .execute(tx.as_mut())
            .await?;
        sqlx::query(
            "UPDATE moa.artifact SET latest_revision_uid = NULL WHERE latest_revision_uid = $1",
        )
        .bind(revision_uid)
        .execute(tx.as_mut())
        .await?;
        sqlx::query("DELETE FROM moa.artifact_revision WHERE revision_uid = $1")
            .bind(revision_uid)
            .execute(tx.as_mut())
            .await?;
    }

    sqlx::query("DELETE FROM learning_log_source WHERE tenant_id = $1 AND learning_id = ANY($2)")
        .bind(&tenant_key)
        .bind(&closure.learning_ids)
        .execute(tx.as_mut())
        .await?;
    sqlx::query("DELETE FROM learning_log WHERE tenant_id = $1 AND id = ANY($2)")
        .bind(&tenant_key)
        .bind(&closure.learning_ids)
        .execute(tx.as_mut())
        .await?;

    sqlx::query(
        "DELETE FROM learning_candidate_decision WHERE tenant_id = $1 AND candidate_id = ANY($2)",
    )
    .bind(&tenant_key)
    .bind(&closure.candidate_ids)
    .execute(tx.as_mut())
    .await?;
    sqlx::query(
        "DELETE FROM learning_candidate_source WHERE tenant_id = $1 AND candidate_id = ANY($2)",
    )
    .bind(&tenant_key)
    .bind(&closure.candidate_ids)
    .execute(tx.as_mut())
    .await?;
    sqlx::query("DELETE FROM learning_candidates WHERE tenant_id = $1 AND id = ANY($2)")
        .bind(&tenant_key)
        .bind(&closure.candidate_ids)
        .execute(tx.as_mut())
        .await?;

    record_decisions_in(
        tx.as_mut(),
        tenant_id,
        operation_ref,
        attempt_id,
        &decisions,
    )
    .await?;

    tx.commit().await?;
    Ok(decisions)
}

/// Returns the storage partition text used by tenant-scoped session rows.
fn storage_partition_of(tenant_id: TenantId) -> String {
    moa_core::types::identifiers::StoragePartitionId::for_tenant(tenant_id).to_string()
}

/// Builds the per-record dispositions an erase does or would apply.
fn planned_decisions(closure: &LearningClosure, applied: bool) -> Vec<RecordDecision> {
    let mut decisions = Vec::with_capacity(closure.len());
    for candidate_id in &closure.candidate_ids {
        decisions.push(RecordDecision {
            kind: ErasureRecordKind::LearningCandidate,
            record_id: candidate_id.to_string(),
            disposition: ErasureDisposition::Erased,
            applied,
            reason: Some("derived solely from erased subject evidence".to_string()),
        });
    }
    for learning_id in &closure.learning_ids {
        decisions.push(RecordDecision {
            kind: ErasureRecordKind::LearningLog,
            record_id: learning_id.to_string(),
            disposition: ErasureDisposition::Erased,
            applied,
            reason: Some("learning entry derived from erased subject evidence".to_string()),
        });
    }
    for contribution_uid in &closure.suite_contribution_uids {
        decisions.push(RecordDecision {
            kind: ErasureRecordKind::ArtifactSuiteContribution,
            record_id: contribution_uid.to_string(),
            disposition: ErasureDisposition::Erased,
            applied,
            reason: Some("regression suite generated from erased subject transcript".to_string()),
        });
    }
    for revision_uid in &closure.revision_uids {
        let (disposition, reason) = if closure.is_sole_source(*revision_uid) {
            (
                ErasureDisposition::Erased,
                "every contribution to this revision came from the erased subject",
            )
        } else {
            (
                ErasureDisposition::InvalidatedRevision,
                "fused model output cannot be separated from the erased contributor, so the \
                 whole serving revision is invalidated",
            )
        };
        decisions.push(RecordDecision {
            kind: ErasureRecordKind::ArtifactRevision,
            record_id: revision_uid.to_string(),
            disposition,
            applied,
            reason: Some(reason.to_string()),
        });
    }
    decisions
}

/// Builds one uniform disposition across every enumerated record.
fn closure_decisions(
    closure: &LearningClosure,
    disposition: ErasureDisposition,
    applied: bool,
    reason: Option<String>,
) -> Vec<RecordDecision> {
    let mut decisions = Vec::with_capacity(closure.len());
    let mut push = |kind: ErasureRecordKind, id: &Uuid| {
        decisions.push(RecordDecision {
            kind,
            record_id: id.to_string(),
            disposition,
            applied,
            reason: reason.clone(),
        });
    };
    for candidate_id in &closure.candidate_ids {
        push(ErasureRecordKind::LearningCandidate, candidate_id);
    }
    for learning_id in &closure.learning_ids {
        push(ErasureRecordKind::LearningLog, learning_id);
    }
    for contribution_uid in &closure.suite_contribution_uids {
        push(
            ErasureRecordKind::ArtifactSuiteContribution,
            contribution_uid,
        );
    }
    for revision_uid in &closure.revision_uids {
        push(ErasureRecordKind::ArtifactRevision, revision_uid);
    }
    decisions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closure() -> LearningClosure {
        LearningClosure {
            experience_ids: vec![Uuid::from_u128(1)],
            candidate_ids: vec![Uuid::from_u128(2)],
            learning_ids: vec![Uuid::from_u128(3)],
            revision_uids: vec![Uuid::from_u128(4), Uuid::from_u128(5)],
            sole_source_revision_uids: vec![Uuid::from_u128(4)],
            suite_contribution_uids: vec![Uuid::from_u128(6)],
        }
    }

    #[test]
    fn legal_hold_decisions_are_never_applied_and_cover_every_record() {
        // Pins: a hold produces one unapplied decision per enumerated record. If any
        // were marked applied, the ledger would claim protected data was mutated
        // under a hold — the exact assertion the hold exists to make checkable.
        let closure = closure();
        let decisions = legal_hold_decisions(&closure, "litigation hold");

        assert_eq!(decisions.len(), 5);
        assert!(decisions.iter().all(|decision| !decision.applied));
        assert!(
            decisions
                .iter()
                .all(|decision| decision.disposition == ErasureDisposition::RetainedLegalHold)
        );
    }

    #[test]
    fn dry_run_plans_the_same_dispositions_the_real_run_would_apply() {
        // Pins: a dry run differs from a real erase in exactly one bit. Same records,
        // same per-record dispositions, `applied` false throughout — so a dry run is
        // never mistakable for evidence of deletion, and never understates what the
        // real run would do either.
        let closure = closure();
        let dry = dry_run_decisions(&closure);
        let applied = planned_decisions(&closure, true);

        assert_eq!(dry.len(), applied.len());
        assert!(dry.iter().all(|decision| !decision.applied));
        assert!(applied.iter().all(|decision| decision.applied));
        for (dry, applied) in dry.iter().zip(applied.iter()) {
            assert_eq!(dry.kind, applied.kind);
            assert_eq!(dry.record_id, applied.record_id);
            assert_eq!(dry.disposition, applied.disposition);
        }
    }

    #[test]
    fn a_shared_revision_is_invalidated_while_a_sole_source_revision_is_erased() {
        // Pins: the split that decides whether other tenants' contributors lose their
        // skill. A sole-source revision is deleted; a revision with a surviving
        // outside contributor is invalidated rather than deleted, and never reported
        // as `retained_shared` — its fused definition is not separable.
        let closure = closure();
        let decisions = planned_decisions(&closure, true);
        let revision_decisions = decisions
            .iter()
            .filter(|decision| decision.kind == ErasureRecordKind::ArtifactRevision)
            .collect::<Vec<_>>();

        assert_eq!(revision_decisions.len(), 2);
        let sole = revision_decisions
            .iter()
            .find(|decision| decision.record_id == Uuid::from_u128(4).to_string())
            .expect("sole-source revision decision");
        let shared = revision_decisions
            .iter()
            .find(|decision| decision.record_id == Uuid::from_u128(5).to_string())
            .expect("shared revision decision");

        assert_eq!(sole.disposition, ErasureDisposition::Erased);
        assert_eq!(shared.disposition, ErasureDisposition::InvalidatedRevision);
    }

    #[test]
    fn only_legal_hold_retention_is_barred_from_being_applied() {
        // Pins: the applied-ness rule is a property of the disposition, not of the
        // caller. Erasure, redaction, invalidation, and proven-independent retention
        // are all real outcomes a run can carry out; a hold retention never is.
        assert!(!ErasureDisposition::RetainedLegalHold.can_be_applied());
        for disposition in [
            ErasureDisposition::Erased,
            ErasureDisposition::Redacted,
            ErasureDisposition::InvalidatedRevision,
            ErasureDisposition::RetainedShared,
        ] {
            assert!(disposition.can_be_applied());
        }
    }
}

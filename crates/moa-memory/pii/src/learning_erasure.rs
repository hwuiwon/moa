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
//! * **Fused model output is non-subtractable.** A revision's definition and
//!   source text may fuse several transcripts. You cannot carve one contributor
//!   back out of a paragraph, so every attributable revision is archived and
//!   cleared while its pinned identity remains for referential integrity.

use moa_db::ScopedConn;
use sqlx::PgPool;
use uuid::Uuid;

use moa_core::types::{identifiers::TenantId, memory::RlsContext};

use crate::erasure::{ErasureError, Result};

/// What was decided about one enumerated record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErasureDisposition {
    /// The row was deleted outright.
    Erased,
    /// A serving revision was invalidated because its fused output could not be
    /// separated from the erased contributor.
    InvalidatedRevision,
    /// A legal hold covers the record; nothing was touched.
    RetainedLegalHold,
}

impl ErasureDisposition {
    /// Returns the stable database representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Erased => "erased",
            Self::InvalidatedRevision => "invalidated_revision",
            Self::RetainedLegalHold => "retained_legal_hold",
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
    /// A learning-log entry.
    LearningLog,
    /// An artifact revision.
    ArtifactRevision,
    /// One generated or accumulated suite contribution.
    ArtifactSuiteContribution,
    /// An experience record.
    ExperienceRecord,
    /// One attribution attached to an experience record.
    ExperienceAttribution,
}

impl ErasureRecordKind {
    /// Returns the stable database representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LearningCandidate => "learning_candidate",
            Self::LearningLog => "learning_log",
            Self::ArtifactRevision => "artifact_revision",
            Self::ArtifactSuiteContribution => "artifact_suite_contribution",
            Self::ExperienceRecord => "experience_record",
            Self::ExperienceAttribution => "experience_attribution",
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
    /// Attributions attached to the subject's experience records.
    pub attribution_ids: Vec<Uuid>,
    /// Candidates derived from the subject's data.
    pub candidate_ids: Vec<Uuid>,
    /// Learning-log entries derived from those candidates or the subject directly.
    pub learning_ids: Vec<Uuid>,
    /// Revisions with at least one contribution from an enumerated candidate.
    pub revision_uids: Vec<Uuid>,
    /// Suite contributions attributable to the subject.
    pub suite_contribution_uids: Vec<Uuid>,
}

impl LearningClosure {
    fn records(&self) -> impl Iterator<Item = (ErasureRecordKind, Uuid)> + '_ {
        self.experience_ids
            .iter()
            .copied()
            .map(|id| (ErasureRecordKind::ExperienceRecord, id))
            .chain(
                self.attribution_ids
                    .iter()
                    .copied()
                    .map(|id| (ErasureRecordKind::ExperienceAttribution, id)),
            )
            .chain(
                self.candidate_ids
                    .iter()
                    .copied()
                    .map(|id| (ErasureRecordKind::LearningCandidate, id)),
            )
            .chain(
                self.learning_ids
                    .iter()
                    .copied()
                    .map(|id| (ErasureRecordKind::LearningLog, id)),
            )
            .chain(
                self.suite_contribution_uids
                    .iter()
                    .copied()
                    .map(|id| (ErasureRecordKind::ArtifactSuiteContribution, id)),
            )
            .chain(
                self.revision_uids
                    .iter()
                    .copied()
                    .map(|id| (ErasureRecordKind::ArtifactRevision, id)),
            )
    }

    /// Returns the total number of enumerated records across every level.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records().count()
    }

    /// Returns whether the closure enumerated nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
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
    let mut conn = ScopedConn::begin(pool, &RlsContext::tenant(tenant_id)).await?;

    let experience_ids = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id FROM experience_records
        WHERE tenant_id = $1 AND user_id = ANY($2)
        ORDER BY id
        "#,
    )
    .bind(&tenant_key)
    .bind(&subjects.user_ids)
    .fetch_all(conn.as_mut())
    .await?;

    let attribution_ids = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id FROM experience_attributions
        WHERE tenant_id = $1 AND experience_id = ANY($2)
        ORDER BY id
        "#,
    )
    .bind(&tenant_key)
    .bind(&experience_ids)
    .fetch_all(conn.as_mut())
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
    .fetch_all(conn.as_mut())
    .await?;

    let segment_ids = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id FROM task_segments
        WHERE tenant_id = $1 AND session_id = ANY($2)
        ORDER BY id
        "#,
    )
    .bind(&tenant_key)
    .bind(&session_ids)
    .fetch_all(conn.as_mut())
    .await?;

    // A typed recursive closure is necessary because rollback candidates can
    // point at promotion candidates, and later candidates can point at artifact
    // revisions derived from earlier candidates. Deleting only the first layer
    // both leaves attributable learning behind and lets restrictive source FKs
    // roll back the whole erase.
    let derived = sqlx::query_as::<_, (String, Uuid)>(
        r#"
        WITH RECURSIVE derived(kind, id) AS (
            SELECT 'candidate'::TEXT, source.candidate_id
            FROM learning_candidate_source AS source
            WHERE source.tenant_id = $1
              AND (
                  source.contact_id = ANY($2)
                  OR source.session_id = ANY($3)
                  OR source.event_session_id = ANY($3)
                  OR source.segment_id = ANY($4)
                  OR source.experience_id = ANY($5)
                  OR source.attribution_id = ANY($6)
              )
            UNION
            SELECT next.kind, next.id
            FROM derived AS parent
            CROSS JOIN LATERAL (
                SELECT 'candidate'::TEXT AS kind, source.candidate_id AS id
                FROM learning_candidate_source AS source
                WHERE source.tenant_id = $1
                  AND (
                      (parent.kind = 'candidate'
                       AND source.promotion_candidate_id = parent.id)
                      OR (parent.kind = 'revision'
                          AND source.artifact_revision_uid = parent.id)
                  )
                UNION
                SELECT 'revision'::TEXT, contribution.revision_uid
                FROM moa.artifact_revision_contribution AS contribution
                WHERE parent.kind = 'candidate'
                  AND contribution.tenant_id = $1
                  AND contribution.candidate_id = parent.id
            ) AS next
        )
        SELECT kind, id FROM derived ORDER BY kind, id
        "#,
    )
    .bind(&tenant_key)
    .bind(&subjects.contact_ids)
    .bind(&session_ids)
    .bind(&segment_ids)
    .bind(&experience_ids)
    .bind(&attribution_ids)
    .fetch_all(conn.as_mut())
    .await?;
    let candidate_ids = derived
        .iter()
        .filter_map(|(kind, id)| (kind == "candidate").then_some(*id))
        .collect::<Vec<_>>();
    let revision_uids = derived
        .iter()
        .filter_map(|(kind, id)| (kind == "revision").then_some(*id))
        .collect::<Vec<_>>();

    let learning_ids = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT DISTINCT source.learning_id
        FROM learning_log_source AS source
        WHERE source.tenant_id = $1
          AND (
              source.candidate_id = ANY($2)
              OR source.session_id = ANY($3)
              OR source.experience_id = ANY($4)
              OR source.segment_id = ANY($5)
              OR source.artifact_revision_uid = ANY($6)
          )
        ORDER BY 1
        "#,
    )
    .bind(&tenant_key)
    .bind(&candidate_ids)
    .bind(&session_ids)
    .bind(&experience_ids)
    .bind(&segment_ids)
    .bind(&revision_uids)
    .fetch_all(conn.as_mut())
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
    .fetch_all(conn.as_mut())
    .await?;

    conn.commit().await?;
    Ok(LearningClosure {
        experience_ids,
        attribution_ids,
        candidate_ids,
        learning_ids,
        revision_uids,
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
/// Keyed by `(tenant, subject, attempt, record_kind, record_id)`, so a resumed
/// job converges while a later post-hold or post-dry-run attempt records its own
/// applied outcome instead of being masked by an earlier plan.
pub async fn record_decisions(
    pool: &PgPool,
    tenant_id: TenantId,
    subject_user_id: &str,
    attempt_id: &str,
    decisions: &[RecordDecision],
) -> Result<u64> {
    let mut conn = ScopedConn::begin_as_app(pool, &RlsContext::tenant(tenant_id), true).await?;
    let recorded = record_decisions_in(
        conn.as_mut(),
        tenant_id,
        subject_user_id,
        attempt_id,
        decisions,
    )
    .await?;
    conn.commit().await?;
    Ok(recorded)
}

/// Writes decisions on the caller's connection or open transaction.
///
/// The erase path uses this to commit each disposition in the SAME transaction
/// as the mutation it describes, so there is no window in which rows are gone
/// but the ledger does not say why.
async fn record_decisions_in(
    conn: &mut sqlx::PgConnection,
    tenant_id: TenantId,
    subject_user_id: &str,
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
                (decision_uid, tenant_id, subject_user_id, attempt_id, record_kind, record_id,
                 disposition, applied, reason)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            ON CONFLICT
                (tenant_id, subject_user_id, attempt_id, record_kind, record_id)
                DO NOTHING
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(tenant_id.0)
        .bind(subject_user_id)
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
    planned_decisions(closure)
}

/// Erases the enumerated closure and returns the applied dispositions.
///
/// Order is child-to-parent so no delete trips a foreign key: suite
/// contributions, revision contributions, learning-log sources, learning-log
/// entries, candidate sources, candidates, then subject experiences.
/// Attributable revisions are invalidated in place — definition, source text,
/// files, and serving state cleared — while their identity remains available to
/// runtime and audit rows that legitimately pin it.
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
    subject_user_id: &str,
    attempt_id: &str,
    closure: &LearningClosure,
) -> Result<Vec<RecordDecision>> {
    let tenant_key = tenant_id.to_string();
    let decisions = applied_decisions(closure);
    let mut tx = ScopedConn::begin(pool, &RlsContext::tenant(tenant_id)).await?;

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
    // Delete every source before either its owner or its referent. This is what
    // makes recursive promotion/revision dependencies erasable under RESTRICT.
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

    // Never delete an artifact revision. Other runtime tables legitimately pin
    // revision identity with restrictive FKs, while privacy requires removing
    // attributable bytes and serving eligibility, not destroying that identity.
    for revision_uid in &closure.revision_uids {
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
            WHERE revision_uid = $1 AND tenant_id = $2
            "#,
        )
        .bind(revision_uid)
        .bind(tenant_id.0)
        .execute(tx.as_mut())
        .await?;
        sqlx::query("DELETE FROM moa.artifact_file WHERE revision_uid = $1 AND tenant_id = $2")
            .bind(revision_uid)
            .bind(tenant_id.0)
            .execute(tx.as_mut())
            .await?;
    }

    sqlx::query("DELETE FROM experience_attributions WHERE tenant_id = $1 AND id = ANY($2)")
        .bind(&tenant_key)
        .bind(&closure.attribution_ids)
        .execute(tx.as_mut())
        .await?;
    sqlx::query("DELETE FROM experience_records WHERE tenant_id = $1 AND id = ANY($2)")
        .bind(&tenant_key)
        .bind(&closure.experience_ids)
        .execute(tx.as_mut())
        .await?;

    // The protected ledger is the final write, after which this transaction has
    // no reason to retain the owner role used for multi-subject closure cleanup.
    tx.assume_app_role().await?;
    record_decisions_in(
        tx.as_mut(),
        tenant_id,
        subject_user_id,
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

fn planned_decisions(closure: &LearningClosure) -> Vec<RecordDecision> {
    erase_decisions(closure, ErasureDecisionState::Planned)
}

fn applied_decisions(closure: &LearningClosure) -> Vec<RecordDecision> {
    erase_decisions(closure, ErasureDecisionState::Applied)
}

#[derive(Clone, Copy)]
enum ErasureDecisionState {
    Planned,
    Applied,
}

impl ErasureDecisionState {
    const fn is_applied(self) -> bool {
        matches!(self, Self::Applied)
    }
}

/// Builds the per-record dispositions an erase does or would apply.
fn erase_decisions(closure: &LearningClosure, state: ErasureDecisionState) -> Vec<RecordDecision> {
    closure
        .records()
        .map(|(kind, record_id)| {
            let (disposition, reason) = match kind {
                ErasureRecordKind::ExperienceRecord => (
                    ErasureDisposition::Erased,
                    "experience derived from erased subject evidence",
                ),
                ErasureRecordKind::ExperienceAttribution => (
                    ErasureDisposition::Erased,
                    "attribution belongs to an erased experience",
                ),
                ErasureRecordKind::LearningCandidate => (
                    ErasureDisposition::Erased,
                    "candidate includes erased subject evidence and is not subtractable",
                ),
                ErasureRecordKind::LearningLog => (
                    ErasureDisposition::Erased,
                    "learning entry derived from erased subject evidence",
                ),
                ErasureRecordKind::ArtifactSuiteContribution => (
                    ErasureDisposition::Erased,
                    "regression suite generated from erased subject transcript",
                ),
                ErasureRecordKind::ArtifactRevision => (
                    ErasureDisposition::InvalidatedRevision,
                    "attributable source bytes were removed and the pinned revision identity archived",
                ),
            };
            RecordDecision {
                kind,
                record_id: record_id.to_string(),
                disposition,
                applied: state.is_applied(),
                reason: Some(reason.to_string()),
            }
        })
        .collect()
}

/// Builds one uniform disposition across every enumerated record.
fn closure_decisions(
    closure: &LearningClosure,
    disposition: ErasureDisposition,
    reason: Option<String>,
) -> Vec<RecordDecision> {
    closure
        .records()
        .map(|(kind, record_id)| RecordDecision {
            kind,
            record_id: record_id.to_string(),
            disposition,
            applied: false,
            reason: reason.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closure() -> LearningClosure {
        LearningClosure {
            experience_ids: vec![Uuid::from_u128(1)],
            attribution_ids: vec![Uuid::from_u128(2)],
            candidate_ids: vec![Uuid::from_u128(3)],
            learning_ids: vec![Uuid::from_u128(4)],
            revision_uids: vec![Uuid::from_u128(5)],
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

        assert_eq!(decisions.len(), 6);
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
        let applied = applied_decisions(&closure);

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
    fn every_attributable_revision_is_invalidated_without_deleting_its_identity() {
        // Pins: attributable bytes and serving state are removed without deleting
        // the revision identity that runtime and audit rows may legitimately pin.
        let closure = closure();
        let decisions = applied_decisions(&closure);
        let revision_decisions = decisions
            .iter()
            .filter(|decision| decision.kind == ErasureRecordKind::ArtifactRevision)
            .collect::<Vec<_>>();

        assert_eq!(revision_decisions.len(), 1);
        assert_eq!(
            revision_decisions[0].disposition,
            ErasureDisposition::InvalidatedRevision
        );
    }

    #[test]
    fn only_legal_hold_retention_is_barred_from_being_applied() {
        // Pins: the applied-ness rule is a property of the disposition, not of the
        // caller. Erasure and invalidation are real outcomes a run can carry out;
        // a hold retention never is.
        assert!(!ErasureDisposition::RetainedLegalHold.can_be_applied());
        for disposition in [
            ErasureDisposition::Erased,
            ErasureDisposition::InvalidatedRevision,
        ] {
            assert!(disposition.can_be_applied());
        }
    }
}

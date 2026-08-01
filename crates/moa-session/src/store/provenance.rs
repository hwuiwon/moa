//! Normalized learning provenance and durable review-decision storage.
//!
//! Everything here exists so two questions have joinable answers instead of
//! array-membership guesses: *what data is this learning derived from*, and
//! *what did a reviewer decide about it*. Both are written in the same
//! transaction as the row they describe, because a provenance row filed
//! afterwards is a provenance row that a crash can lose while the derived
//! learning survives — which is precisely the shape that let erasure delete a
//! source memory and leave attributable learning standing.

use super::*;

use std::collections::HashMap;

use serde_json::Value;
use sqlx::PgConnection;

use moa_core::types::experience::{LearningCandidateDecisionRecord, LearningCandidateSourceRef};
use moa_core::types::learning::LearningLogSourceRef;

use crate::queries::tenant_id_from_storage;

/// Columns selected when reading one normalized candidate source back.
const CANDIDATE_SOURCE_COLUMNS: &str = "source_kind, experience_id, attribution_id, session_id, \
     event_id, event_session_id, segment_id, contact_id, promotion_candidate_id, \
     artifact_revision_uid, experiment_run_uid, experiment_trial_uid, score_run_id";

/// Columns selected when reading one normalized learning-log source back.
const LEARNING_LOG_SOURCE_COLUMNS: &str =
    "source_kind, candidate_id, experience_id, session_id, segment_id, artifact_revision_uid";

impl PostgresSessionStore {
    /// Writes every normalized source for one candidate in the caller's transaction.
    ///
    /// Insertion inherits `tenant_id`, `storage_partition_id`, and `user_id`
    /// from the candidate row itself rather than accepting them as parameters,
    /// so a caller cannot file a source under a tenant the candidate does not
    /// belong to even by mistake. The composite foreign keys then reject a
    /// referent that lives in a different partition.
    pub async fn append_learning_candidate_sources_in_tx(
        &self,
        conn: &mut PgConnection,
        candidate_id: Uuid,
        sources: &[LearningCandidateSourceRef],
    ) -> Result<()> {
        if sources.is_empty() {
            return Err(MoaError::StorageError(format!(
                "learning candidate `{candidate_id}` has no normalized sources; a candidate that \
                 cannot be attributed cannot be erased or explained"
            )));
        }
        let learning_candidates = self.table_name("learning_candidates");
        let learning_candidate_source = self.table_name("learning_candidate_source");
        for reference in sources {
            let columns = CandidateSourceColumns::from(reference);
            sqlx::query(&format!(
                "INSERT INTO {learning_candidate_source} \
                 (id, candidate_id, tenant_id, storage_partition_id, user_id, source_kind, \
                  experience_id, attribution_id, session_id, event_id, event_session_id, \
                  segment_id, contact_id, promotion_candidate_id, artifact_revision_uid, \
                  experiment_run_uid, experiment_trial_uid, score_run_id) \
                 SELECT $1, candidate.id, candidate.tenant_id, candidate.storage_partition_id, \
                        candidate.user_id, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15 \
                 FROM {learning_candidates} AS candidate \
                 WHERE candidate.id = $2 \
                 ON CONFLICT DO NOTHING"
            ))
            .bind(Uuid::now_v7())
            .bind(candidate_id)
            .bind(reference.kind())
            .bind(columns.experience_id)
            .bind(columns.attribution_id)
            .bind(columns.session_id)
            .bind(columns.event_id)
            .bind(columns.event_session_id)
            .bind(columns.segment_id)
            .bind(columns.contact_id)
            .bind(columns.promotion_candidate_id)
            .bind(columns.artifact_revision_uid)
            .bind(columns.experiment_run_uid)
            .bind(columns.experiment_trial_uid)
            .bind(columns.score_run_id)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        }
        Ok(())
    }

    /// Reads every normalized source behind one candidate.
    pub async fn list_learning_candidate_sources(
        &self,
        candidate_id: Uuid,
    ) -> Result<Vec<LearningCandidateSourceRef>> {
        let learning_candidate_source = self.table_name("learning_candidate_source");
        let rows = sqlx::query(&format!(
            "SELECT {CANDIDATE_SOURCE_COLUMNS} FROM {learning_candidate_source} \
             WHERE candidate_id = $1 ORDER BY source_kind, created_at, id"
        ))
        .bind(candidate_id)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        rows.iter().map(candidate_source_from_row).collect()
    }

    /// Writes every normalized source for one learning-log entry.
    pub async fn append_learning_log_sources_in_tx(
        &self,
        conn: &mut PgConnection,
        learning_id: Uuid,
        sources: &[LearningLogSourceRef],
    ) -> Result<()> {
        if sources.is_empty() {
            return Err(MoaError::StorageError(format!(
                "learning-log entry `{learning_id}` has no normalized sources; an unattributable \
                 learning entry cannot be erased or explained"
            )));
        }
        let learning_log = self.table_name("learning_log");
        let learning_log_source = self.table_name("learning_log_source");
        for reference in sources {
            let columns = LearningLogSourceColumns::from(reference);
            sqlx::query(&format!(
                "INSERT INTO {learning_log_source} \
                 (id, learning_id, tenant_id, storage_partition_id, user_id, source_kind, \
                  candidate_id, experience_id, session_id, segment_id, artifact_revision_uid) \
                 SELECT $1, entry.id, entry.tenant_id, entry.storage_partition_id, entry.user_id, \
                        $3, $4, $5, $6, $7, $8 \
                 FROM {learning_log} AS entry \
                 WHERE entry.id = $2 \
                 ON CONFLICT DO NOTHING"
            ))
            .bind(Uuid::now_v7())
            .bind(learning_id)
            .bind(reference.kind())
            .bind(columns.candidate_id)
            .bind(columns.experience_id)
            .bind(columns.session_id)
            .bind(columns.segment_id)
            .bind(columns.artifact_revision_uid)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        }
        Ok(())
    }

    /// Reads every normalized source behind one learning-log entry.
    pub async fn list_learning_log_sources(
        &self,
        learning_id: Uuid,
    ) -> Result<Vec<LearningLogSourceRef>> {
        let learning_log_source = self.table_name("learning_log_source");
        let rows = sqlx::query(&format!(
            "SELECT {LEARNING_LOG_SOURCE_COLUMNS} FROM {learning_log_source} \
             WHERE learning_id = $1 ORDER BY source_kind, created_at, id"
        ))
        .bind(learning_id)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        rows.iter().map(learning_log_source_from_row).collect()
    }

    /// Fills in the normalized sources for a batch of already-loaded candidates.
    ///
    /// Row mappers deliberately return candidates with empty `sources`: the
    /// one-of columns do not fit a flat projection, and templating them into the
    /// shared column constant would hard-code an unqualified table name that
    /// breaks under a test schema. One extra keyed query per read is the honest
    /// cost of keeping the projection literal.
    pub async fn hydrate_learning_candidate_sources(
        &self,
        candidates: &mut [LearningCandidate],
    ) -> Result<()> {
        if candidates.is_empty() {
            return Ok(());
        }
        let learning_candidate_source = self.table_name("learning_candidate_source");
        let candidate_ids = candidates
            .iter()
            .map(|candidate| candidate.id)
            .collect::<Vec<_>>();
        let rows = sqlx::query(&format!(
            "SELECT candidate_id, {CANDIDATE_SOURCE_COLUMNS} FROM {learning_candidate_source} \
             WHERE candidate_id = ANY($1) ORDER BY source_kind, created_at, id"
        ))
        .bind(&candidate_ids)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        let mut by_candidate: HashMap<Uuid, Vec<LearningCandidateSourceRef>> = HashMap::new();
        for row in &rows {
            let candidate_id = row.col::<Uuid>("candidate_id")?;
            by_candidate
                .entry(candidate_id)
                .or_default()
                .push(candidate_source_from_row(row)?);
        }
        for candidate in candidates.iter_mut() {
            candidate.sources = by_candidate.remove(&candidate.id).unwrap_or_default();
        }
        Ok(())
    }

    /// Fills in the normalized sources for a batch of already-loaded learning entries.
    pub async fn hydrate_learning_entry_sources(
        &self,
        entries: &mut [LearningEntry],
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let learning_log_source = self.table_name("learning_log_source");
        let learning_ids = entries.iter().map(|entry| entry.id).collect::<Vec<_>>();
        let rows = sqlx::query(&format!(
            "SELECT learning_id, {LEARNING_LOG_SOURCE_COLUMNS} FROM {learning_log_source} \
             WHERE learning_id = ANY($1) ORDER BY source_kind, created_at, id"
        ))
        .bind(&learning_ids)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        let mut by_entry: HashMap<Uuid, Vec<LearningLogSourceRef>> = HashMap::new();
        for row in &rows {
            let learning_id = row.col::<Uuid>("learning_id")?;
            by_entry
                .entry(learning_id)
                .or_default()
                .push(learning_log_source_from_row(row)?);
        }
        for entry in entries.iter_mut() {
            entry.sources = by_entry.remove(&entry.id).unwrap_or_default();
        }
        Ok(())
    }

    /// Records one durable review decision, returning false when it already existed.
    ///
    /// Idempotent by `(candidate, decision)`, so a Restate re-execution of the
    /// same review writes exactly one audit rather than a second identical row.
    /// The caller uses the returned flag to distinguish a first application from
    /// a replay without having to read the table back.
    pub async fn record_learning_candidate_decision_in_tx(
        &self,
        conn: &mut PgConnection,
        decision: &LearningCandidateDecisionRecord,
    ) -> Result<bool> {
        let learning_candidates = self.table_name("learning_candidates");
        let learning_candidate_decision = self.table_name("learning_candidate_decision");
        let inserted = sqlx::query(&format!(
            "INSERT INTO {learning_candidate_decision} \
             (id, candidate_id, tenant_id, storage_partition_id, user_id, decision, \
              from_status, to_status, reviewer_subject, reason, request_digest, outcome, \
              decided_at) \
             SELECT $1, candidate.id, candidate.tenant_id, candidate.storage_partition_id, \
                    candidate.user_id, $3, $4, $5, $6, $7, $8, $9, $10 \
             FROM {learning_candidates} AS candidate \
             WHERE candidate.id = $2 \
             ON CONFLICT (candidate_id, decision) DO NOTHING"
        ))
        .bind(decision.id)
        .bind(decision.candidate_id)
        .bind(decision.decision.as_str())
        .bind(decision.from_status.as_str())
        .bind(decision.to_status.as_str())
        .bind(decision.reviewer_subject.as_deref())
        .bind(decision.reason.as_deref())
        .bind(decision.request_digest.as_deref())
        .bind(decision.outcome.as_ref().map(sqlx::types::Json))
        .bind(decision.decided_at)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        Ok(inserted > 0)
    }

    /// Lists the durable review history for one candidate.
    pub async fn list_learning_candidate_decisions(
        &self,
        candidate_id: Uuid,
    ) -> Result<Vec<LearningCandidateDecisionRecord>> {
        let learning_candidate_decision = self.table_name("learning_candidate_decision");
        let rows = sqlx::query(&format!(
            "SELECT id, candidate_id, tenant_id, decision, from_status, to_status, \
                    reviewer_subject, reason, request_digest, outcome, decided_at \
             FROM {learning_candidate_decision} \
             WHERE candidate_id = $1 ORDER BY decided_at, id"
        ))
        .bind(candidate_id)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        rows.iter().map(decision_from_row).collect()
    }
}

/// Typed reference flattened into the one-of columns it occupies.
#[derive(Default)]
struct CandidateSourceColumns {
    experience_id: Option<Uuid>,
    attribution_id: Option<Uuid>,
    session_id: Option<Uuid>,
    event_id: Option<Uuid>,
    event_session_id: Option<Uuid>,
    segment_id: Option<Uuid>,
    contact_id: Option<Uuid>,
    promotion_candidate_id: Option<Uuid>,
    artifact_revision_uid: Option<Uuid>,
    experiment_run_uid: Option<Uuid>,
    experiment_trial_uid: Option<Uuid>,
    score_run_id: Option<Uuid>,
}

impl From<&LearningCandidateSourceRef> for CandidateSourceColumns {
    fn from(reference: &LearningCandidateSourceRef) -> Self {
        let mut columns = Self::default();
        match reference {
            LearningCandidateSourceRef::Experience { experience_id } => {
                columns.experience_id = Some(*experience_id);
            }
            LearningCandidateSourceRef::Attribution { attribution_id } => {
                columns.attribution_id = Some(*attribution_id);
            }
            LearningCandidateSourceRef::Session { session_id } => {
                columns.session_id = Some(session_id.0);
            }
            LearningCandidateSourceRef::Event {
                event_id,
                session_id,
            } => {
                columns.event_id = Some(*event_id);
                columns.event_session_id = Some(session_id.0);
            }
            LearningCandidateSourceRef::TaskSegment { segment_id } => {
                columns.segment_id = Some(segment_id.0);
            }
            LearningCandidateSourceRef::Contact { contact_id } => {
                columns.contact_id = Some(contact_id.0);
            }
            LearningCandidateSourceRef::PromotionCandidate { candidate_id } => {
                columns.promotion_candidate_id = Some(*candidate_id);
            }
            LearningCandidateSourceRef::ArtifactRevision { revision_uid } => {
                columns.artifact_revision_uid = Some(*revision_uid);
            }
            LearningCandidateSourceRef::ExperimentRun { run_uid } => {
                columns.experiment_run_uid = Some(*run_uid);
            }
            LearningCandidateSourceRef::ExperimentTrial { trial_uid } => {
                columns.experiment_trial_uid = Some(*trial_uid);
            }
            LearningCandidateSourceRef::ScoreRun { run_id } => {
                columns.score_run_id = Some(*run_id);
            }
        }
        columns
    }
}

#[derive(Default)]
struct LearningLogSourceColumns {
    candidate_id: Option<Uuid>,
    experience_id: Option<Uuid>,
    session_id: Option<Uuid>,
    segment_id: Option<Uuid>,
    artifact_revision_uid: Option<Uuid>,
}

impl From<&LearningLogSourceRef> for LearningLogSourceColumns {
    fn from(reference: &LearningLogSourceRef) -> Self {
        let mut columns = Self::default();
        match reference {
            LearningLogSourceRef::Candidate { candidate_id } => {
                columns.candidate_id = Some(*candidate_id);
            }
            LearningLogSourceRef::Experience { experience_id } => {
                columns.experience_id = Some(*experience_id);
            }
            LearningLogSourceRef::Session { session_id } => {
                columns.session_id = Some(session_id.0);
            }
            LearningLogSourceRef::TaskSegment { segment_id } => {
                columns.segment_id = Some(segment_id.0);
            }
            LearningLogSourceRef::ArtifactRevision { revision_uid } => {
                columns.artifact_revision_uid = Some(*revision_uid);
            }
        }
        columns
    }
}

fn candidate_source_from_row(row: &sqlx::postgres::PgRow) -> Result<LearningCandidateSourceRef> {
    let kind = row.col::<String>("source_kind")?;
    let required = |column: &'static str| -> Result<Uuid> {
        row.col::<Option<Uuid>>(column)?.ok_or_else(|| {
            MoaError::StorageError(format!(
                "learning candidate source of kind `{kind}` is missing its `{column}` referent"
            ))
        })
    };
    match kind.as_str() {
        "experience" => Ok(LearningCandidateSourceRef::Experience {
            experience_id: required("experience_id")?,
        }),
        "attribution" => Ok(LearningCandidateSourceRef::Attribution {
            attribution_id: required("attribution_id")?,
        }),
        "session" => Ok(LearningCandidateSourceRef::Session {
            session_id: SessionId(required("session_id")?),
        }),
        "event" => Ok(LearningCandidateSourceRef::Event {
            event_id: required("event_id")?,
            session_id: SessionId(required("event_session_id")?),
        }),
        "task_segment" => Ok(LearningCandidateSourceRef::TaskSegment {
            segment_id: SegmentId(required("segment_id")?),
        }),
        "contact" => Ok(LearningCandidateSourceRef::Contact {
            contact_id: ContactId(required("contact_id")?),
        }),
        "promotion_candidate" => Ok(LearningCandidateSourceRef::PromotionCandidate {
            candidate_id: required("promotion_candidate_id")?,
        }),
        "artifact_revision" => Ok(LearningCandidateSourceRef::ArtifactRevision {
            revision_uid: required("artifact_revision_uid")?,
        }),
        "experiment_run" => Ok(LearningCandidateSourceRef::ExperimentRun {
            run_uid: required("experiment_run_uid")?,
        }),
        "experiment_trial" => Ok(LearningCandidateSourceRef::ExperimentTrial {
            trial_uid: required("experiment_trial_uid")?,
        }),
        "score_run" => Ok(LearningCandidateSourceRef::ScoreRun {
            run_id: required("score_run_id")?,
        }),
        other => Err(MoaError::StorageError(format!(
            "unknown learning candidate source kind `{other}`"
        ))),
    }
}

fn learning_log_source_from_row(row: &sqlx::postgres::PgRow) -> Result<LearningLogSourceRef> {
    let kind = row.col::<String>("source_kind")?;
    let required = |column: &'static str| -> Result<Uuid> {
        row.col::<Option<Uuid>>(column)?.ok_or_else(|| {
            MoaError::StorageError(format!(
                "learning-log source of kind `{kind}` is missing its `{column}` referent"
            ))
        })
    };
    match kind.as_str() {
        "candidate" => Ok(LearningLogSourceRef::Candidate {
            candidate_id: required("candidate_id")?,
        }),
        "experience" => Ok(LearningLogSourceRef::Experience {
            experience_id: required("experience_id")?,
        }),
        "session" => Ok(LearningLogSourceRef::Session {
            session_id: SessionId(required("session_id")?),
        }),
        "task_segment" => Ok(LearningLogSourceRef::TaskSegment {
            segment_id: SegmentId(required("segment_id")?),
        }),
        "artifact_revision" => Ok(LearningLogSourceRef::ArtifactRevision {
            revision_uid: required("artifact_revision_uid")?,
        }),
        other => Err(MoaError::StorageError(format!(
            "unknown learning-log source kind `{other}`"
        ))),
    }
}

fn decision_from_row(row: &sqlx::postgres::PgRow) -> Result<LearningCandidateDecisionRecord> {
    Ok(LearningCandidateDecisionRecord {
        id: row.col::<Uuid>("id")?,
        candidate_id: row.col::<Uuid>("candidate_id")?,
        tenant_id: tenant_id_from_storage(row.col::<String>("tenant_id")?),
        decision: from_db("learning review decision", &row.col::<String>("decision")?)?,
        from_status: from_db(
            "learning candidate status",
            &row.col::<String>("from_status")?,
        )?,
        to_status: from_db(
            "learning candidate status",
            &row.col::<String>("to_status")?,
        )?,
        reviewer_subject: row.col::<Option<String>>("reviewer_subject")?,
        reason: row.col::<Option<String>>("reason")?,
        request_digest: row.col::<Option<Vec<u8>>>("request_digest")?,
        outcome: row
            .col::<Option<sqlx::types::Json<Value>>>("outcome")?
            .map(|value| value.0),
        decided_at: row.col::<DateTime<Utc>>("decided_at")?,
    })
}

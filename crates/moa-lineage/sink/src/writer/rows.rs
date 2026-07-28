//! Conversion from lineage events into journal and storage row shapes.

use chrono::{DateTime, Utc};
use moa_lineage_core::chain::canonical_payload_hash;
use moa_lineage_core::{
    ExperimentScoreProvenance, ExperimentScoreTarget, LineageEvent, ScoreRecord, ScoreSource,
    ScoreTarget, ScoreValue,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::Result;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "table", content = "row", rename_all = "snake_case")]
#[allow(
    clippy::large_enum_variant,
    reason = "batches are dominated by lineage rows; boxing the rarer score row would add an allocation per journal decode to save nothing on the hot variant"
)]
pub(crate) enum PendingRow {
    Lineage(LineageRow),
    Score(ScoreRow),
}

impl PendingRow {
    pub(crate) fn from_event(evt: LineageEvent) -> Result<Self> {
        match evt {
            LineageEvent::Eval(record) => Ok(Self::Score(ScoreRow::from_record(record))),
            other => Ok(Self::Lineage(LineageRow::from_event(other)?)),
        }
    }

    /// Returns the purge and erasure scope this row belongs to.
    ///
    /// The acceptance queue stores it as a column so tenant purge, subject
    /// erasure, and the destruction fence can all work on the queue without
    /// decoding any payload.
    pub(super) fn storage_partition_id(&self) -> String {
        match self {
            Self::Lineage(row) => row.storage_partition_id.clone(),
            Self::Score(row) => row.storage_partition_id.clone(),
        }
    }

    /// Returns the subject this row belongs to, when it has one.
    pub(super) fn user_id(&self) -> Option<String> {
        match self {
            Self::Lineage(row) => Some(row.user_id.clone()),
            Self::Score(row) => row.user_id.clone(),
        }
    }

    /// Returns the row's session, when it has one.
    pub(super) fn session_id(&self) -> Option<Uuid> {
        match self {
            Self::Lineage(row) => Some(row.session_id),
            Self::Score(row) => row.session_id,
        }
    }

    /// Returns the row's turn, when it has one.
    pub(super) fn turn_id(&self) -> Option<Uuid> {
        match self {
            Self::Lineage(row) => Some(row.turn_id),
            Self::Score(row) => row.turn_id,
        }
    }
}

/// Returns the queue's `event_class` discriminator for a pending row.
///
/// Kept exhaustive over the enum rather than derived from a string so adding a
/// third row shape is a compile error here and a `CHECK` violation in the
/// database, instead of a row that silently lands under the wrong class.
pub(super) fn pending_row_event_class(row: &PendingRow) -> &'static str {
    match row {
        PendingRow::Lineage(_) => "lineage",
        PendingRow::Score(_) => "score",
    }
}

pub(super) fn decode_pending_row(
    payload: serde_json::Value,
) -> std::result::Result<PendingRow, serde_json::Error> {
    serde_json::from_value::<PendingRow>(payload.clone())
        .or_else(|_| serde_json::from_value::<LineageRow>(payload).map(PendingRow::Lineage))
}

/// Journaled `turn_lineage` row shared by the Postgres and ClickHouse writers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct LineageRow {
    pub(crate) turn_id: Uuid,
    pub(crate) session_id: Uuid,
    pub(crate) user_id: String,
    pub(crate) storage_partition_id: String,
    pub(crate) ts: DateTime<Utc>,
    pub(crate) tier: i16,
    pub(crate) record_kind: i16,
    pub(crate) payload: serde_json::Value,
    pub(crate) integrity_hash: Vec<u8>,
    pub(crate) prev_hash: Option<Vec<u8>>,
}

impl LineageRow {
    fn from_event(evt: LineageEvent) -> Result<Self> {
        let payload = serde_json::to_value(&evt)?;
        let integrity_hash = canonical_payload_hash(&payload)?.as_bytes().to_vec();
        let record_kind = evt.record_kind().as_i16();
        let fallback_ts = Utc::now();

        let (turn_id, session_id, user_id, storage_partition_id, ts) = match &evt {
            LineageEvent::Retrieval(record) => (
                record.turn_id.0,
                record.session_id.0,
                record.user_id.to_string(),
                record.storage_partition_id.to_string(),
                record.ts,
            ),
            LineageEvent::Context(record) => (
                record.turn_id.0,
                record.session_id.0,
                record.user_id.to_string(),
                record.storage_partition_id.to_string(),
                record.ts,
            ),
            LineageEvent::Generation(record) => (
                record.turn_id.0,
                record.session_id.0,
                record.user_id.to_string(),
                record.storage_partition_id.to_string(),
                record.ts,
            ),
            LineageEvent::Citation(record) => (
                record.turn_id.0,
                record.session_id.0,
                record.user_id.to_string(),
                record.storage_partition_id.to_string(),
                record.ts,
            ),
            LineageEvent::Decision(record) => (
                record.turn_id.0,
                record.session_id.0,
                record.user_id.to_string(),
                record.storage_partition_id.to_string(),
                record.ts,
            ),
            LineageEvent::Eval(_) => (
                Uuid::now_v7(),
                Uuid::nil(),
                "unknown".to_string(),
                "unknown".to_string(),
                fallback_ts,
            ),
        };

        Ok(Self {
            turn_id,
            session_id,
            user_id,
            storage_partition_id,
            ts,
            tier: 1,
            record_kind,
            payload,
            integrity_hash,
            prev_hash: None,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ScoreRow {
    pub(super) score_id: Uuid,
    pub(super) ts: DateTime<Utc>,
    pub(super) storage_partition_id: String,
    pub(super) user_id: Option<String>,
    pub(super) target_kind: String,
    pub(super) turn_id: Option<Uuid>,
    pub(super) session_id: Option<Uuid>,
    pub(super) run_id: Option<Uuid>,
    pub(super) item_id: Option<Uuid>,
    pub(super) dataset_id: Option<Uuid>,
    pub(super) name: String,
    pub(super) value_type: String,
    pub(super) value_numeric: Option<f64>,
    pub(super) value_boolean: Option<bool>,
    pub(super) value_categorical: Option<String>,
    pub(super) source: String,
    pub(super) model_or_evaluator: String,
    pub(super) comment: Option<String>,
    pub(super) provenance: Option<ExperimentScoreProvenanceRow>,
}

/// Journaled `moa.experiment_score_provenance` row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct ExperimentScoreProvenanceRow {
    pub(super) score_id: Uuid,
    pub(super) storage_partition_id: String,
    pub(super) user_id: Option<String>,
    pub(super) score_run_id: Uuid,
    pub(super) experiment_run_uid: Uuid,
    pub(super) plan_revision_uid: Uuid,
    pub(super) trial_uid: Uuid,
    pub(super) target_session_id: Option<Uuid>,
    pub(super) target_execution_run_uid: Option<Uuid>,
    pub(super) evaluator_id: String,
    pub(super) evaluator_version: String,
    pub(super) score_name: String,
    pub(super) value_type: String,
    pub(super) evidence_ref: String,
    pub(super) evidence_hash: Vec<u8>,
}

impl ScoreRow {
    fn from_record(record: ScoreRecord) -> Self {
        let (target_kind, turn_id, session_id, target_run_id, item_id) = match record.target {
            ScoreTarget::Turn { turn_id } => {
                ("turn".to_string(), Some(turn_id.0), None, None, None)
            }
            ScoreTarget::Session { session_id } => {
                ("session".to_string(), None, Some(session_id.0), None, None)
            }
            ScoreTarget::DatasetRunItem { run_id, item_id } => (
                "dataset_run_item".to_string(),
                None,
                None,
                Some(run_id),
                Some(item_id),
            ),
        };
        let (value_type, value_numeric, value_boolean, value_categorical) = match record.value {
            ScoreValue::Numeric(value) => ("numeric".to_string(), Some(value), None, None),
            ScoreValue::Boolean(value) => ("boolean".to_string(), None, Some(value), None),
            ScoreValue::Categorical(value) => ("categorical".to_string(), None, None, Some(value)),
        };

        let storage_partition_id = record.storage_partition_id.to_string();
        let user_id = record.user_id.map(|user_id| user_id.to_string());
        let run_id = record.run_id.or(target_run_id);
        let provenance = record.experiment_provenance.map(|provenance| {
            experiment_provenance_row(
                record.score_id,
                storage_partition_id.clone(),
                user_id.clone(),
                run_id,
                provenance,
            )
        });

        Self {
            score_id: record.score_id,
            ts: record.ts,
            storage_partition_id,
            user_id,
            target_kind,
            turn_id,
            session_id,
            run_id,
            item_id,
            dataset_id: record.dataset_id,
            name: record.name,
            value_type,
            value_numeric,
            value_boolean,
            value_categorical,
            source: score_source_to_db(record.source).to_string(),
            model_or_evaluator: record.model_or_evaluator,
            comment: record.comment,
            provenance,
        }
    }
}

fn experiment_provenance_row(
    score_id: Uuid,
    storage_partition_id: String,
    user_id: Option<String>,
    score_run_id: Option<Uuid>,
    provenance: ExperimentScoreProvenance,
) -> ExperimentScoreProvenanceRow {
    let (target_session_id, target_execution_run_uid) = match provenance.target {
        ExperimentScoreTarget::Session { session_id } => (Some(session_id.0), None),
        ExperimentScoreTarget::ExecutionRun { execution_run_uid } => {
            (None, Some(execution_run_uid))
        }
    };
    ExperimentScoreProvenanceRow {
        score_id,
        storage_partition_id,
        user_id,
        // A provenance-bearing score without a run id would violate the score-run
        // foreign key. The nil UUID makes that a loud constraint failure at write
        // time rather than a quiet row that satisfies nothing at read time.
        score_run_id: score_run_id.unwrap_or(Uuid::nil()),
        experiment_run_uid: provenance.experiment_run_uid,
        plan_revision_uid: provenance.plan_revision_uid,
        trial_uid: provenance.trial_uid,
        target_session_id,
        target_execution_run_uid,
        evaluator_id: provenance.evaluator_id,
        evaluator_version: provenance.evaluator_version,
        score_name: provenance.score_name,
        value_type: provenance.value_type,
        evidence_ref: provenance.evidence_ref,
        evidence_hash: provenance.evidence_hash,
    }
}

fn score_source_to_db(source: ScoreSource) -> &'static str {
    match source {
        ScoreSource::OnlineJudge => "online_judge",
        ScoreSource::OfflineReplay => "offline_replay",
        ScoreSource::Human => "human",
        ScoreSource::External => "external",
        ScoreSource::ProductEvaluator => "product_evaluator",
    }
}

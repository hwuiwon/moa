//! Conversion from lineage events into journal and storage row shapes.

use chrono::{DateTime, Utc};
use moa_lineage_core::chain::canonical_payload_hash;
use moa_lineage_core::{LineageEvent, ScoreRecord, ScoreSource, ScoreTarget, ScoreValue};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::Result;

pub(super) fn pending_row_ts(row: &PendingRow) -> DateTime<Utc> {
    match row {
        PendingRow::Lineage(row) => row.ts,
        PendingRow::Score(row) => row.ts,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "table", content = "row", rename_all = "snake_case")]
pub(super) enum PendingRow {
    Lineage(LineageRow),
    Score(ScoreRow),
}

impl PendingRow {
    pub(super) fn from_event(evt: LineageEvent) -> Result<Self> {
        match evt {
            LineageEvent::Eval(record) => Ok(Self::Score(ScoreRow::from_record(record))),
            other => Ok(Self::Lineage(LineageRow::from_event(other)?)),
        }
    }
}

pub(super) fn decode_pending_row(
    payload: &[u8],
) -> std::result::Result<PendingRow, serde_json::Error> {
    serde_json::from_slice::<PendingRow>(payload)
        .or_else(|_| serde_json::from_slice::<LineageRow>(payload).map(PendingRow::Lineage))
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
pub(super) struct ScoreRow {
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

        Self {
            score_id: record.score_id,
            ts: record.ts,
            storage_partition_id: record.storage_partition_id.to_string(),
            user_id: record.user_id.map(|user_id| user_id.to_string()),
            target_kind,
            turn_id,
            session_id,
            run_id: record.run_id.or(target_run_id),
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
        }
    }
}

fn score_source_to_db(source: ScoreSource) -> &'static str {
    match source {
        ScoreSource::OnlineJudge => "online_judge",
        ScoreSource::OfflineReplay => "offline_replay",
        ScoreSource::Human => "human",
        ScoreSource::External => "external",
    }
}

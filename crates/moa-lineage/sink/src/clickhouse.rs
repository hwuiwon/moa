//! ClickHouse-backed store for high-volume `turn_lineage` rows.
//!
//! When `[clickhouse]` is configured, the lineage writer lands `turn_lineage`
//! rows here instead of Postgres/Timescale. Scores, dead letters, and the
//! compliance chain state stay in Postgres: scores are SQL-joined against
//! OLTP experiment tables, and the audit hash chain needs transactional
//! folds that a column store cannot provide.

use chrono::{DateTime, Utc};
use clickhouse::sql::Identifier;
use clickhouse::{Client, Row};
use moa_core::config::ClickHouseConfig;
use moa_core::wire::lineage::LineageRecordView;
use moa_core::{
    types::identifiers::SessionId, types::identifiers::StoragePartitionId,
    types::identifiers::TenantId, types::identifiers::UserId,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::writer::LineageRow;

/// ClickHouse connection plus the schema knobs needed at startup.
#[derive(Clone)]
pub struct ClickHouseStore {
    client: Client,
    database: String,
    lineage_ttl_days: u32,
}

impl std::fmt::Debug for ClickHouseStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClickHouseStore")
            .field("database", &self.database)
            .field("lineage_ttl_days", &self.lineage_ttl_days)
            .finish_non_exhaustive()
    }
}

/// Wire row for `turn_lineage`; field names must match the column names.
#[derive(Debug, Row, Serialize, Deserialize)]
struct ClickHouseLineageRow {
    #[serde(with = "clickhouse::serde::uuid")]
    turn_id: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    session_id: Uuid,
    user_id: String,
    storage_partition_id: String,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    ts: DateTime<Utc>,
    tier: i16,
    record_kind: i16,
    payload: String,
    answer_text: Option<String>,
    integrity_hash: String,
    prev_hash: Option<String>,
}

impl ClickHouseLineageRow {
    fn from_lineage_row(row: &LineageRow) -> Self {
        Self {
            turn_id: row.turn_id,
            session_id: row.session_id,
            user_id: row.user_id.clone(),
            storage_partition_id: row.storage_partition_id.clone(),
            ts: row.ts,
            tier: row.tier,
            record_kind: row.record_kind,
            payload: row.payload.to_string(),
            answer_text: None,
            integrity_hash: hex_lower(&row.integrity_hash),
            prev_hash: row.prev_hash.as_deref().map(hex_lower),
        }
    }
}

/// Read row shared by the explain/typed-query/trace helpers.
#[derive(Debug, Row, Deserialize)]
struct ClickHouseRecordRow {
    #[serde(with = "clickhouse::serde::uuid")]
    turn_id: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    session_id: Uuid,
    user_id: String,
    storage_partition_id: String,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    ts: DateTime<Utc>,
    record_kind: i16,
    payload: String,
}

impl ClickHouseRecordRow {
    fn into_view(self) -> Result<LineageRecordView> {
        let tenant_id = Uuid::parse_str(&self.storage_partition_id)
            .map(TenantId::from)
            .map_err(|error| {
                Error::Invalid(format!(
                    "lineage storage_partition_id is not a tenant id: {error}"
                ))
            })?;
        Ok(LineageRecordView {
            turn_id: self.turn_id,
            session_id: Some(SessionId(self.session_id)),
            tenant_id: Some(tenant_id),
            user_id: Some(UserId::new(self.user_id)),
            ts: self.ts,
            record_kind: self.record_kind,
            payload: serde_json::from_str(&self.payload)?,
            summary: None,
        })
    }
}

impl ClickHouseStore {
    /// Builds a store from the validated `[clickhouse]` config section.
    #[must_use]
    pub fn connect(config: &ClickHouseConfig) -> Self {
        let mut client = Client::default().with_url(config.url.trim());
        if let Some(user) = config.user.as_deref() {
            client = client.with_user(user);
        }
        if let Some(password) = config.password.as_deref() {
            client = client.with_password(password);
        }
        Self {
            client,
            database: config.database.trim().to_string(),
            lineage_ttl_days: config.lineage_ttl_days,
        }
    }

    /// Creates the database and `turn_lineage` table when missing.
    ///
    /// `ReplacingMergeTree` over the Postgres upsert key keeps journal replays
    /// idempotent; the TTL mirrors the Timescale retention drop. The bloom
    /// filter skip indexes keep explain/DSAR/trace reads — which filter by
    /// `session_id`/`turn_id`, neither in the primary key — from scanning a
    /// tenant's whole retention window.
    pub async fn ensure_schema(&self) -> Result<()> {
        self.client
            .query("CREATE DATABASE IF NOT EXISTS ?")
            .bind(Identifier(&self.database))
            .execute()
            .await?;
        self.client
            .query(
                "CREATE TABLE IF NOT EXISTS ?.turn_lineage (
                    turn_id UUID,
                    session_id UUID,
                    user_id String,
                    storage_partition_id String,
                    ts DateTime64(6, 'UTC'),
                    tier Int16,
                    record_kind Int16,
                    payload String,
                    answer_text Nullable(String),
                    integrity_hash String,
                    prev_hash Nullable(String),
                    INDEX idx_turn_lineage_session session_id TYPE bloom_filter(0.01) GRANULARITY 4,
                    INDEX idx_turn_lineage_turn turn_id TYPE bloom_filter(0.01) GRANULARITY 4
                )
                ENGINE = ReplacingMergeTree
                PARTITION BY toYYYYMMDD(ts)
                ORDER BY (storage_partition_id, ts, turn_id, record_kind)
                TTL toDateTime(ts) + INTERVAL ? DAY",
            )
            .bind(Identifier(&self.database))
            .bind(self.lineage_ttl_days)
            .execute()
            .await?;
        Ok(())
    }

    /// Appends one flush batch of lineage rows.
    pub(crate) async fn insert_lineage_rows(&self, rows: &[LineageRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut insert = self
            .client
            .insert::<ClickHouseLineageRow>(&self.qualified("turn_lineage"))?;
        for row in rows {
            insert
                .write(&ClickHouseLineageRow::from_lineage_row(row))
                .await?;
        }
        insert.end().await?;
        Ok(())
    }

    /// Loads lineage records for one tenant-scoped session or turn id,
    /// mirroring the Postgres `explain_records` shape and ordering.
    pub async fn explain_records(
        &self,
        storage_partition_id: &StoragePartitionId,
        id: Uuid,
    ) -> Result<Vec<LineageRecordView>> {
        let rows = self
            .client
            .query(
                "SELECT turn_id, session_id, user_id, storage_partition_id, ts, record_kind, payload
                 FROM ?.turn_lineage
                 WHERE storage_partition_id = ? AND (session_id = ? OR turn_id = ?)
                 ORDER BY ts ASC, record_kind ASC",
            )
            .bind(Identifier(&self.database))
            .bind(storage_partition_id.as_str())
            .bind(id)
            .bind(id)
            .fetch_all::<ClickHouseRecordRow>()
            .await?;
        rows.into_iter()
            .map(ClickHouseRecordRow::into_view)
            .collect()
    }

    /// Loads a tenant's lineage records filtered by the typed query fields,
    /// mirroring the edge route's Postgres query builder.
    pub async fn query_records(
        &self,
        storage_partition_id: &StoragePartitionId,
        filters: LineageQueryFilters<'_>,
    ) -> Result<Vec<LineageQueryRecord>> {
        let mut sql = String::from(
            "SELECT turn_id, session_id, user_id, ts, record_kind, payload, answer_text
             FROM ?.turn_lineage
             WHERE storage_partition_id = ?",
        );
        if filters.turn_id.is_some() {
            sql.push_str(" AND turn_id = ?");
        }
        if filters.session_id.is_some() {
            sql.push_str(" AND session_id = ?");
        }
        if filters.user_id.is_some() {
            sql.push_str(" AND user_id = ?");
        }
        if filters.record_kind.is_some() {
            sql.push_str(" AND record_kind = ?");
        }
        if filters.from_time.is_some() {
            sql.push_str(" AND ts >= fromUnixTimestamp64Micro(?, 'UTC')");
        }
        if filters.to_time.is_some() {
            sql.push_str(" AND ts <= fromUnixTimestamp64Micro(?, 'UTC')");
        }
        sql.push_str(if filters.descending {
            " ORDER BY ts DESC, record_kind ASC, turn_id ASC LIMIT ?"
        } else {
            " ORDER BY ts ASC, record_kind ASC, turn_id ASC LIMIT ?"
        });

        let mut query = self
            .client
            .query(&sql)
            .bind(Identifier(&self.database))
            .bind(storage_partition_id.as_str());
        if let Some(turn_id) = filters.turn_id {
            query = query.bind(turn_id);
        }
        if let Some(session_id) = filters.session_id {
            query = query.bind(session_id);
        }
        if let Some(user_id) = filters.user_id {
            query = query.bind(user_id);
        }
        if let Some(record_kind) = filters.record_kind {
            query = query.bind(record_kind);
        }
        if let Some(from_time) = filters.from_time {
            query = query.bind(from_time.timestamp_micros());
        }
        if let Some(to_time) = filters.to_time {
            query = query.bind(to_time.timestamp_micros());
        }
        let rows = query
            .bind(filters.limit)
            .fetch_all::<ClickHouseQueryRow>()
            .await?;
        rows.into_iter()
            .map(ClickHouseQueryRow::into_record)
            .collect()
    }

    /// Loads `(ts, payload)` pairs for one turn and record kind, mirroring the
    /// knowledge query-trace read.
    pub async fn trace_payloads(
        &self,
        storage_partition_id: &StoragePartitionId,
        turn_id: Uuid,
        record_kind: i16,
    ) -> Result<Vec<(DateTime<Utc>, serde_json::Value)>> {
        #[derive(Row, Deserialize)]
        struct TraceRow {
            #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
            ts: DateTime<Utc>,
            payload: String,
        }

        let rows = self
            .client
            .query(
                "SELECT ts, payload
                 FROM ?.turn_lineage
                 WHERE storage_partition_id = ? AND turn_id = ? AND record_kind = ?
                 ORDER BY ts ASC, record_kind ASC",
            )
            .bind(Identifier(&self.database))
            .bind(storage_partition_id.as_str())
            .bind(turn_id)
            .bind(record_kind)
            .fetch_all::<TraceRow>()
            .await?;
        rows.into_iter()
            .map(|row| Ok((row.ts, serde_json::from_str(&row.payload)?)))
            .collect()
    }

    /// Loads lineage rows whose payload contains a DSAR subject string,
    /// mirroring the Postgres DSAR export shape.
    pub async fn load_dsar_export_records(
        &self,
        storage_partition_id: &StoragePartitionId,
        subject: &str,
    ) -> Result<Vec<serde_json::Value>> {
        let rows = self
            .client
            .query(
                "SELECT turn_id, session_id, user_id, storage_partition_id, ts, record_kind,
                        payload, integrity_hash, prev_hash
                 FROM ?.turn_lineage
                 WHERE storage_partition_id = ? AND positionCaseInsensitive(payload, ?) > 0
                 ORDER BY ts ASC, turn_id ASC, record_kind ASC
                 LIMIT 10000",
            )
            .bind(Identifier(&self.database))
            .bind(storage_partition_id.as_str())
            .bind(subject)
            .fetch_all::<ClickHouseDsarRow>()
            .await?;
        rows.into_iter().map(ClickHouseDsarRow::into_json).collect()
    }

    /// Deletes a tenant partition's lineage rows during offboarding.
    pub async fn delete_partition_rows(
        &self,
        storage_partition_id: &StoragePartitionId,
    ) -> Result<()> {
        self.client
            .query("DELETE FROM ?.turn_lineage WHERE storage_partition_id = ?")
            .bind(Identifier(&self.database))
            .bind(storage_partition_id.as_str())
            .execute()
            .await?;
        Ok(())
    }

    fn qualified(&self, table: &str) -> String {
        format!("{}.{table}", self.database)
    }
}

/// Filters for the typed lineage query, matching the edge route's fields.
#[derive(Debug, Clone, Copy, Default)]
pub struct LineageQueryFilters<'a> {
    /// Exact turn id.
    pub turn_id: Option<Uuid>,
    /// Exact session id.
    pub session_id: Option<Uuid>,
    /// Exact user id.
    pub user_id: Option<&'a str>,
    /// Exact record kind.
    pub record_kind: Option<i16>,
    /// Inclusive lower timestamp bound.
    pub from_time: Option<DateTime<Utc>>,
    /// Inclusive upper timestamp bound.
    pub to_time: Option<DateTime<Utc>>,
    /// Whether results order newest-first.
    pub descending: bool,
    /// Row limit, already clamped by the caller.
    pub limit: i64,
}

/// One typed-query result row: the record view plus its answer text summary.
#[derive(Debug)]
pub struct LineageQueryRecord {
    /// Turn id of the row.
    pub turn_id: Uuid,
    /// Session id of the row.
    pub session_id: Uuid,
    /// User id of the row.
    pub user_id: String,
    /// Row timestamp.
    pub ts: DateTime<Utc>,
    /// Lineage record kind.
    pub record_kind: i16,
    /// Decoded JSON payload.
    pub payload: serde_json::Value,
    /// Optional answer text summary.
    pub answer_text: Option<String>,
}

/// Typed-query read row including the answer text.
#[derive(Row, Deserialize)]
struct ClickHouseQueryRow {
    #[serde(with = "clickhouse::serde::uuid")]
    turn_id: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    session_id: Uuid,
    user_id: String,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    ts: DateTime<Utc>,
    record_kind: i16,
    payload: String,
    answer_text: Option<String>,
}

impl ClickHouseQueryRow {
    fn into_record(self) -> Result<LineageQueryRecord> {
        Ok(LineageQueryRecord {
            turn_id: self.turn_id,
            session_id: self.session_id,
            user_id: self.user_id,
            ts: self.ts,
            record_kind: self.record_kind,
            payload: serde_json::from_str(&self.payload)?,
            answer_text: self.answer_text,
        })
    }
}

/// DSAR export row including the integrity hashes.
#[derive(Row, Deserialize)]
struct ClickHouseDsarRow {
    #[serde(with = "clickhouse::serde::uuid")]
    turn_id: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    session_id: Uuid,
    user_id: String,
    storage_partition_id: String,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    ts: DateTime<Utc>,
    record_kind: i16,
    payload: String,
    integrity_hash: String,
    prev_hash: Option<String>,
}

impl ClickHouseDsarRow {
    fn into_json(self) -> Result<serde_json::Value> {
        let payload: serde_json::Value = serde_json::from_str(&self.payload)?;
        Ok(serde_json::json!({
            "turn_id": self.turn_id,
            "session_id": self.session_id,
            "user_id": self.user_id,
            "storage_partition_id": self.storage_partition_id,
            "ts": self.ts,
            "record_kind": self.record_kind,
            "payload": payload,
            "integrity_hash": self.integrity_hash,
            "prev_hash": self.prev_hash,
        }))
    }
}

/// Whether a ClickHouse write failure is worth retrying.
pub(crate) fn is_retryable_clickhouse_error(error: &clickhouse::error::Error) -> bool {
    matches!(
        error,
        clickhouse::error::Error::Network(_) | clickhouse::error::Error::TimedOut
    )
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn lineage_row() -> LineageRow {
        LineageRow {
            turn_id: Uuid::now_v7(),
            session_id: Uuid::now_v7(),
            user_id: "user-1".to_string(),
            storage_partition_id: "partition-1".to_string(),
            ts: Utc.with_ymd_and_hms(2026, 7, 8, 12, 0, 0).unwrap(),
            tier: 1,
            record_kind: 1,
            payload: serde_json::json!({"kind": "retrieval"}),
            integrity_hash: vec![0xab, 0x01],
            prev_hash: Some(vec![0xcd, 0xef]),
        }
    }

    #[test]
    fn wire_row_hex_encodes_hashes_and_serializes_payload_as_json_text() {
        // Pins: the ClickHouse wire row carries the same identity fields as the
        // Postgres COPY path, with hashes hex-encoded and payload as JSON text.
        let row = lineage_row();

        let wire = ClickHouseLineageRow::from_lineage_row(&row);

        assert_eq!(wire.turn_id, row.turn_id);
        assert_eq!(wire.session_id, row.session_id);
        assert_eq!(wire.storage_partition_id, "partition-1");
        assert_eq!(wire.payload, "{\"kind\":\"retrieval\"}");
        assert_eq!(wire.integrity_hash, "ab01");
        assert_eq!(wire.prev_hash.as_deref(), Some("cdef"));
        assert_eq!(wire.answer_text, None);
    }

    #[test]
    fn record_row_rejects_non_tenant_partition_ids() {
        // Pins: explain reads fail loudly when the partition id is not a tenant
        // uuid instead of fabricating a tenant.
        let row = ClickHouseRecordRow {
            turn_id: Uuid::now_v7(),
            session_id: Uuid::now_v7(),
            user_id: "user-1".to_string(),
            storage_partition_id: "not-a-uuid".to_string(),
            ts: Utc::now(),
            record_kind: 1,
            payload: "{}".to_string(),
        };

        let error = row.into_view().expect_err("non-uuid partition should fail");
        assert!(error.to_string().contains("not a tenant id"));
    }

    #[test]
    fn clickhouse_retry_classification_is_transport_aware() {
        // Pins: transient transport failures retry; schema/data mismatches do not.
        assert!(is_retryable_clickhouse_error(
            &clickhouse::error::Error::TimedOut
        ));
        assert!(!is_retryable_clickhouse_error(
            &clickhouse::error::Error::NotEnoughData
        ));
        assert!(!is_retryable_clickhouse_error(
            &clickhouse::error::Error::BadResponse("Code: 60".to_string())
        ));
    }
}

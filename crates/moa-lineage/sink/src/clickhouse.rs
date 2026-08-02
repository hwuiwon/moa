//! ClickHouse read adapter retained for existing lineage query consumers.
//!
//! Durable writes are Postgres-only so storing content and dequeuing its
//! accepted journal row remain one transaction.

use chrono::{DateTime, Utc};
use clickhouse::sql::Identifier;
use clickhouse::{Client, Row};
use moa_config::ClickHouseConfig;
use moa_core::{
    types::identifiers::SessionId, types::identifiers::StoragePartitionId,
    types::identifiers::TenantId, types::identifiers::UserId,
};
use moa_wire::lineage::LineageRecordView;
use serde::Deserialize;
use uuid::Uuid;

use crate::error::{Error, Result};

/// ClickHouse connection plus the schema knobs needed at startup.
#[derive(Clone)]
pub struct ClickHouseStore {
    client: Client,
    database: String,
}

impl std::fmt::Debug for ClickHouseStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClickHouseStore")
            .field("database", &self.database)
            .finish_non_exhaustive()
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
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
}

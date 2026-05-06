//! Periodic cold-tier exporter worker.

use std::sync::Arc;
use std::time::Duration;

use arrow::array::{
    ArrayRef, FixedSizeBinaryBuilder, Int16Array, RecordBatch, StringArray,
    TimestampMicrosecondArray,
};
use chrono::{Datelike, Utc};
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, PutPayload};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use sqlx::{PgPool, Row};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::schema::lineage_arrow_schema;
use crate::{Error, Result};

const PROGRESS_DDL: &str = r#"
CREATE SCHEMA IF NOT EXISTS analytics;

CREATE TABLE IF NOT EXISTS analytics.lineage_export_progress (
    workspace_id  TEXT        NOT NULL,
    day           DATE        NOT NULL,
    last_ts       TIMESTAMPTZ NOT NULL,
    rows_exported BIGINT      NOT NULL,
    parquet_uri   TEXT        NOT NULL,
    PRIMARY KEY (workspace_id, day)
);
"#;

/// Cold-tier exporter configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColdTierConfig {
    /// Destination object-store bucket name.
    pub bucket: String,
    /// Object key prefix, for example `v1`.
    pub prefix: String,
    /// Poll interval for the worker.
    pub roll_interval: Duration,
    /// Target file size in megabytes before rolling to the next object.
    pub roll_size_mb: u64,
    /// Export rows older than this threshold.
    pub source_age_threshold_hours: u64,
    /// Whether the key includes a workspace partition.
    pub partition_by_workspace: bool,
    /// ZSTD compression level used by the Parquet writer.
    pub zstd_level: i32,
}

impl Default for ColdTierConfig {
    fn default() -> Self {
        Self {
            bucket: "moa-lineage".to_string(),
            prefix: "v1".to_string(),
            roll_interval: Duration::from_secs(30),
            roll_size_mb: 50,
            source_age_threshold_hours: 23,
            partition_by_workspace: true,
            zstd_level: 3,
        }
    }
}

/// Exporter counters for the latest run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExporterStats {
    /// Number of rows selected from the hot tier.
    pub rows_scanned: u64,
    /// Number of rows written to cold storage.
    pub rows_exported: u64,
    /// Number of objects created.
    pub files_written: u64,
}

/// Periodic exporter from `analytics.turn_lineage` to cold object storage.
#[derive(Clone)]
pub struct ColdTierExporter {
    pool: PgPool,
    store: Arc<dyn ObjectStore>,
    config: ColdTierConfig,
}

impl ColdTierExporter {
    /// Creates a cold-tier exporter.
    pub fn new(pool: PgPool, store: Arc<dyn ObjectStore>, config: ColdTierConfig) -> Result<Self> {
        if config.bucket.trim().is_empty() {
            return Err(Error::Config("bucket must not be empty".to_string()));
        }
        if config.prefix.trim().is_empty() {
            return Err(Error::Config("prefix must not be empty".to_string()));
        }
        Ok(Self {
            pool,
            store,
            config,
        })
    }

    /// Runs the exporter until cancellation.
    pub async fn run(self, cancel: CancellationToken) {
        let mut interval = tokio::time::interval(self.config.roll_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    if let Err(error) = self.flush_one_window().await {
                        tracing::error!(%error, "cold-tier flush failed");
                    }
                }
            }
        }
    }

    /// Ensures the export progress bookkeeping table exists.
    pub async fn ensure_progress_table(&self) -> Result<()> {
        sqlx::query(PROGRESS_DDL).execute(&self.pool).await?;
        Ok(())
    }

    /// Flushes one export window and returns export statistics.
    pub async fn flush_one_window(&self) -> Result<ExporterStats> {
        self.ensure_progress_table().await?;
        let rows = sqlx::query(
            r#"
            SELECT turn_id,
                   session_id,
                   user_id,
                   workspace_id,
                   ts,
                   tier,
                   record_kind,
                   payload::text AS payload,
                   answer_text,
                   integrity_hash,
                   prev_hash
              FROM analytics.turn_lineage
             WHERE ts < now() - ($1::text)::interval
             ORDER BY workspace_id ASC, ts ASC
             LIMIT 10000
            "#,
        )
        .bind(format!("{} hours", self.config.source_age_threshold_hours))
        .fetch_all(&self.pool)
        .await?;

        if rows.is_empty() {
            return Ok(ExporterStats::default());
        }

        let first = &rows[0];
        let workspace_id: String = first.get("workspace_id");
        let ts: chrono::DateTime<Utc> = first.get("ts");
        let key = partition_key(
            &self.config.prefix,
            self.config.partition_by_workspace,
            &workspace_id,
            ts,
        );
        let body = render_parquet_rows(&rows, self.config.zstd_level)?;
        self.store
            .put(&ObjectPath::from(key.clone()), PutPayload::from(body))
            .await?;

        let Some(last) = rows.last() else {
            return Ok(ExporterStats::default());
        };
        let last_ts: chrono::DateTime<Utc> = last.get("ts");
        let day = last_ts.date_naive();
        sqlx::query(
            r#"
            INSERT INTO analytics.lineage_export_progress (
                workspace_id, day, last_ts, rows_exported, parquet_uri
            )
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (workspace_id, day) DO UPDATE
            SET last_ts = EXCLUDED.last_ts,
                rows_exported = analytics.lineage_export_progress.rows_exported + EXCLUDED.rows_exported,
                parquet_uri = EXCLUDED.parquet_uri
            "#,
        )
        .bind(&workspace_id)
        .bind(day)
        .bind(last_ts)
        .bind(rows.len() as i64)
        .bind(format!("s3://{}/{}", self.config.bucket, key))
        .execute(&self.pool)
        .await?;

        Ok(ExporterStats {
            rows_scanned: rows.len() as u64,
            rows_exported: rows.len() as u64,
            files_written: 1,
        })
    }
}

/// Builds the Hive-style cold-tier key for one roll.
#[must_use]
pub fn partition_key(
    prefix: &str,
    partition_by_workspace: bool,
    workspace_id: &str,
    ts: chrono::DateTime<Utc>,
) -> String {
    let id = Uuid::now_v7();
    let date = format!("{:04}-{:02}-{:02}", ts.year(), ts.month(), ts.day());
    let base = prefix.trim_matches('/');
    if partition_by_workspace {
        format!(
            "{base}/workspace_id={workspace_id}/dt={date}/{}-{id}.parquet",
            ts.timestamp_millis()
        )
    } else {
        format!("{base}/dt={date}/{}-{id}.parquet", ts.timestamp_millis())
    }
}

fn render_parquet_rows(rows: &[sqlx::postgres::PgRow], zstd_level: i32) -> Result<Vec<u8>> {
    let schema = lineage_arrow_schema();
    let mut turn_ids = FixedSizeBinaryBuilder::with_capacity(rows.len(), 16);
    let mut session_ids = FixedSizeBinaryBuilder::with_capacity(rows.len(), 16);
    let mut user_ids = Vec::with_capacity(rows.len());
    let mut workspace_ids = Vec::with_capacity(rows.len());
    let mut timestamps = Vec::with_capacity(rows.len());
    let mut tiers = Vec::with_capacity(rows.len());
    let mut record_kinds = Vec::with_capacity(rows.len());
    let mut payloads = Vec::with_capacity(rows.len());
    let mut answer_texts = Vec::with_capacity(rows.len());
    let mut integrity_hashes = FixedSizeBinaryBuilder::with_capacity(rows.len(), 32);
    let mut prev_hashes = FixedSizeBinaryBuilder::with_capacity(rows.len(), 32);

    for row in rows {
        let turn_id: Uuid = row.get("turn_id");
        let session_id: Uuid = row.get("session_id");
        turn_ids.append_value(turn_id.as_bytes())?;
        session_ids.append_value(session_id.as_bytes())?;
        user_ids.push(row.get::<String, _>("user_id"));
        workspace_ids.push(row.get::<String, _>("workspace_id"));
        let ts: chrono::DateTime<Utc> = row.get("ts");
        timestamps.push(ts.timestamp_micros());
        tiers.push(row.get::<i16, _>("tier"));
        record_kinds.push(row.get::<i16, _>("record_kind"));
        payloads.push(row.get::<String, _>("payload"));
        answer_texts.push(row.get::<Option<String>, _>("answer_text"));

        let integrity_hash: Vec<u8> = row.get("integrity_hash");
        integrity_hashes.append_value(integrity_hash)?;
        if let Some(prev_hash) = row.get::<Option<Vec<u8>>, _>("prev_hash") {
            prev_hashes.append_value(prev_hash)?;
        } else {
            prev_hashes.append_null();
        }
    }

    let arrays: Vec<ArrayRef> = vec![
        Arc::new(turn_ids.finish()),
        Arc::new(session_ids.finish()),
        Arc::new(StringArray::from(user_ids)),
        Arc::new(StringArray::from(workspace_ids)),
        Arc::new(TimestampMicrosecondArray::from(timestamps).with_timezone("UTC")),
        Arc::new(Int16Array::from(tiers)),
        Arc::new(Int16Array::from(record_kinds)),
        Arc::new(StringArray::from(payloads)),
        Arc::new(StringArray::from(answer_texts)),
        Arc::new(integrity_hashes.finish()),
        Arc::new(prev_hashes.finish()),
    ];
    let batch = RecordBatch::try_new(schema, arrays)?;
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(zstd_level)?))
        .build();
    let mut buffer = Vec::new();
    {
        let mut writer = ArrowWriter::try_new(&mut buffer, batch.schema(), Some(props))?;
        writer.write(&batch)?;
        writer.close()?;
    }
    Ok(buffer)
}

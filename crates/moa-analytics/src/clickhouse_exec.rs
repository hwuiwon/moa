//! ClickHouse execution backend for compiled analytics queries.
//!
//! Analytics queries have arbitrary column sets, so results cannot use a static
//! `clickhouse::Row` derive. The executor requests `JSONEachRow` via
//! [`clickhouse::query::Query::fetch_bytes`] — a self-describing text format —
//! and decodes each line into [`AnalyticsCell`]s using the compiled column
//! kinds. Timestamps are emitted by the compiler as microsecond epochs so they
//! decode to a stable integer regardless of the server datetime format.

use chrono::DateTime;
use clickhouse::Client;
use moa_core::config::ClickHouseConfig;
use moa_core::wire::analytics::{AnalyticsCell, AnalyticsColumn, AnalyticsFieldKind};

use crate::compiler::{AnalyticsBindValue, CompiledAnalyticsQuery};
use crate::error::{Error, Result};

/// Output format requested from ClickHouse for dynamic result decoding.
const RESULT_FORMAT: &str = "JSONEachRow";

/// Whether a ClickHouse error is an UNKNOWN_TABLE (code 60) response.
fn is_unknown_table(error: &clickhouse::error::Error) -> bool {
    matches!(
        error,
        clickhouse::error::Error::BadResponse(message) if message.contains("Code: 60")
    )
}

/// ClickHouse client for the analytics query backend.
///
/// Built from the same `[clickhouse]` config as the lineage store; the target
/// database is pinned on the client so compiled SQL uses unqualified table
/// names.
#[derive(Clone)]
pub struct AnalyticsClickHouseClient {
    client: Client,
    max_execution_time_secs: u64,
    max_rows_to_read: u64,
    max_bytes_to_read: u64,
}

/// Default ClickHouse per-query budgets applied when the caller does not
/// override them from config. Mirror [`moa_core::config::AnalyticsConfig`].
const DEFAULT_MAX_EXECUTION_TIME_SECS: u64 = 10;
const DEFAULT_MAX_ROWS_TO_READ: u64 = 1_000_000_000;
const DEFAULT_MAX_BYTES_TO_READ: u64 = 10_000_000_000;

impl std::fmt::Debug for AnalyticsClickHouseClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnalyticsClickHouseClient")
            .finish_non_exhaustive()
    }
}

impl AnalyticsClickHouseClient {
    /// Builds an analytics client from the validated `[clickhouse]` config.
    #[must_use]
    pub fn connect(config: &ClickHouseConfig) -> Self {
        let mut client = Client::default()
            .with_url(config.url.trim())
            .with_database(config.database.trim());
        if let Some(user) = config.user.as_deref() {
            client = client.with_user(user);
        }
        if let Some(password) = config.password.as_deref() {
            client = client.with_password(password);
        }
        Self::from_client(client)
    }

    /// Wraps an existing client, used by tests to point at a mock server.
    #[must_use]
    pub fn from_client(client: Client) -> Self {
        Self {
            client,
            max_execution_time_secs: DEFAULT_MAX_EXECUTION_TIME_SECS,
            max_rows_to_read: DEFAULT_MAX_ROWS_TO_READ,
            max_bytes_to_read: DEFAULT_MAX_BYTES_TO_READ,
        }
    }

    /// Overrides the per-query ClickHouse budgets (`max_execution_time`,
    /// `max_rows_to_read`, `max_bytes_to_read`) so a runaway read is bounded.
    #[must_use]
    pub fn with_query_budgets(
        mut self,
        max_execution_time_secs: u64,
        max_rows_to_read: u64,
        max_bytes_to_read: u64,
    ) -> Self {
        self.max_execution_time_secs = max_execution_time_secs;
        self.max_rows_to_read = max_rows_to_read;
        self.max_bytes_to_read = max_bytes_to_read;
        self
    }

    /// Deletes a tenant's rows from every analytics table during offboarding.
    ///
    /// Tables are the analytics-export set from
    /// `docs/schemas/clickhouse-analytics.md`; missing tables (export never
    /// ran) are skipped rather than failing the purge.
    pub async fn purge_tenant(&self, tenant_id: uuid::Uuid) -> Result<()> {
        const TABLES: [&str; 10] = [
            "events_raw",
            "dim_sessions",
            "dim_session_agent_context",
            "dim_task_segments",
            "dim_execution_runs",
            "dim_execution_tasks",
            "dim_learning_candidates",
            "dim_experiment_run",
            "turn_fact",
            "tool_call_fact",
        ];
        for table in TABLES {
            let result = self
                .client
                .query("DELETE FROM ? WHERE tenant_id = ?")
                .bind(clickhouse::sql::Identifier(table))
                .bind(tenant_id)
                .execute()
                .await;
            match result {
                Ok(()) => {}
                Err(error) if is_unknown_table(&error) => {
                    tracing::debug!(table, "analytics purge skipped missing table");
                }
                Err(error) => return Err(Error::ClickHouse(error.to_string())),
            }
        }
        Ok(())
    }

    /// Executes a compiled ClickHouse query and decodes result rows.
    pub async fn execute(
        &self,
        compiled: &CompiledAnalyticsQuery,
    ) -> Result<Vec<Vec<AnalyticsCell>>> {
        // Bound the read: a runaway scan is cancelled by ClickHouse instead of
        // reading a tenant's full history to satisfy an exact ordered percentile.
        let mut query = self
            .client
            .query(&compiled.sql)
            .with_option(
                "max_execution_time",
                self.max_execution_time_secs.to_string(),
            )
            .with_option("max_rows_to_read", self.max_rows_to_read.to_string())
            .with_option("max_bytes_to_read", self.max_bytes_to_read.to_string());
        for bind in &compiled.bind_values {
            query = match bind {
                AnalyticsBindValue::String(value) => query.bind(value),
                AnalyticsBindValue::Integer(value) => query.bind(*value),
                AnalyticsBindValue::Float(value) => query.bind(*value),
                AnalyticsBindValue::Bool(value) => query.bind(*value),
                // No analytics field is JSON-typed for ClickHouse; bind the text
                // form defensively so a future JSON filter still produces a value.
                AnalyticsBindValue::Json(value) => query.bind(value.to_string()),
            };
        }

        let bytes = query
            .fetch_bytes(RESULT_FORMAT)
            .map_err(|error| Error::ClickHouse(error.to_string()))?
            .collect()
            .await
            .map_err(|error| Error::ClickHouse(error.to_string()))?;

        decode_json_each_row(&bytes, &compiled.columns)
    }
}

/// Decodes a `JSONEachRow` response body into analytics cell rows.
///
/// Each non-empty line is one JSON object keyed by the compiled SQL aliases
/// (`c0`, `c1`, ...); cells are decoded positionally by the column's kind.
pub(crate) fn decode_json_each_row(
    bytes: &[u8],
    columns: &[AnalyticsColumn],
) -> Result<Vec<Vec<AnalyticsCell>>> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| Error::ClickHouse(format!("response was not utf-8: {error}")))?;
    let mut rows = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let object: serde_json::Map<String, serde_json::Value> = serde_json::from_str(line)
            .map_err(|error| Error::ClickHouse(format!("row is not a json object: {error}")))?;
        let mut cells = Vec::with_capacity(columns.len());
        for (index, column) in columns.iter().enumerate() {
            let alias = format!("c{index}");
            let value = object.get(&alias).unwrap_or(&serde_json::Value::Null);
            cells.push(decode_cell(value, column.kind)?);
        }
        rows.push(cells);
    }
    Ok(rows)
}

fn decode_cell(value: &serde_json::Value, kind: AnalyticsFieldKind) -> Result<AnalyticsCell> {
    if value.is_null() {
        return Ok(AnalyticsCell::Null);
    }
    match kind {
        AnalyticsFieldKind::String | AnalyticsFieldKind::Uuid => value
            .as_str()
            .map(|value| AnalyticsCell::String(value.to_string()))
            .ok_or_else(|| type_error("string", value)),
        AnalyticsFieldKind::Integer => decode_i64(value)
            .map(|value| AnalyticsCell::Number(serde_json::Number::from(value)))
            .ok_or_else(|| type_error("integer", value)),
        AnalyticsFieldKind::Float => decode_f64(value)
            .map(|value| {
                serde_json::Number::from_f64(value)
                    .map(AnalyticsCell::Number)
                    .unwrap_or(AnalyticsCell::Null)
            })
            .ok_or_else(|| type_error("float", value)),
        AnalyticsFieldKind::Boolean => decode_bool(value)
            .map(AnalyticsCell::Bool)
            .ok_or_else(|| type_error("boolean", value)),
        AnalyticsFieldKind::Timestamp => {
            let micros = decode_i64(value).ok_or_else(|| type_error("timestamp micros", value))?;
            let timestamp = DateTime::from_timestamp_micros(micros).ok_or_else(|| {
                Error::ClickHouse(format!("timestamp micros {micros} out of range"))
            })?;
            Ok(AnalyticsCell::String(timestamp.to_rfc3339()))
        }
        AnalyticsFieldKind::Json => Ok(AnalyticsCell::Json(value.clone())),
    }
}

/// Reads a signed integer from a JSON number or a quoted 64-bit integer string.
///
/// ClickHouse quotes 64-bit integers in JSON by default, so `count()` and
/// `toUnixTimestamp64Micro` land here as strings.
fn decode_i64(value: &serde_json::Value) -> Option<i64> {
    match value {
        serde_json::Value::Number(number) => number.as_i64(),
        serde_json::Value::String(text) => text.parse::<i64>().ok(),
        _ => None,
    }
}

fn decode_f64(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(number) => number.as_f64(),
        serde_json::Value::String(text) => text.parse::<f64>().ok(),
        _ => None,
    }
}

/// Reads a boolean from a JSON bool, a `0`/`1` number, or a `"0"`/`"1"` string.
///
/// Native `Bool` columns render as `true`/`false`, but boolean expressions such
/// as `error IS NOT NULL` render as `UInt8` `0`/`1`.
fn decode_bool(value: &serde_json::Value) -> Option<bool> {
    match value {
        serde_json::Value::Bool(value) => Some(*value),
        serde_json::Value::Number(number) => number.as_i64().map(|value| value != 0),
        serde_json::Value::String(text) => match text.as_str() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn type_error(expected: &str, value: &serde_json::Value) -> Error {
    Error::ClickHouse(format!("expected {expected} cell, got {value}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use moa_core::wire::analytics::AnalyticsFieldRole;

    fn column(kind: AnalyticsFieldKind) -> AnalyticsColumn {
        AnalyticsColumn {
            id: "c".to_string(),
            label: "c".to_string(),
            kind,
            role: AnalyticsFieldRole::Dimension,
        }
    }

    #[test]
    fn decodes_all_cell_kinds_including_quoted_ints_and_micros_offline() {
        // Pins: JSONEachRow decoding handles strings, quoted 64-bit integers,
        // floats, native/expression booleans, timestamp micros, and nulls.
        let columns = vec![
            column(AnalyticsFieldKind::String),
            column(AnalyticsFieldKind::Uuid),
            column(AnalyticsFieldKind::Integer),
            column(AnalyticsFieldKind::Float),
            column(AnalyticsFieldKind::Boolean),
            column(AnalyticsFieldKind::Boolean),
            column(AnalyticsFieldKind::Timestamp),
            column(AnalyticsFieldKind::Integer),
        ];
        // 1970-01-01T00:00:01Z == 1_000_000 micros. Integer c2 is quoted as
        // ClickHouse does for 64-bit ints; c5 is a UInt8 boolean expression.
        let body = "{\"c0\":\"chat\",\"c1\":\"018f-uuid\",\"c2\":\"42\",\"c3\":1.5,\
             \"c4\":true,\"c5\":1,\"c6\":\"1000000\",\"c7\":null}\n";

        let rows = decode_json_each_row(body.as_bytes(), &columns).expect("decode rows");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], AnalyticsCell::String("chat".to_string()));
        assert_eq!(rows[0][1], AnalyticsCell::String("018f-uuid".to_string()));
        assert_eq!(
            rows[0][2],
            AnalyticsCell::Number(serde_json::Number::from(42))
        );
        assert_eq!(
            rows[0][3],
            AnalyticsCell::Number(serde_json::Number::from_f64(1.5).unwrap())
        );
        assert_eq!(rows[0][4], AnalyticsCell::Bool(true));
        assert_eq!(rows[0][5], AnalyticsCell::Bool(true));
        assert_eq!(
            rows[0][6],
            AnalyticsCell::String("1970-01-01T00:00:01+00:00".to_string())
        );
        assert_eq!(rows[0][7], AnalyticsCell::Null);
    }

    #[test]
    fn missing_column_key_decodes_as_null_offline() {
        // Pins: a column absent from the row object (e.g. an all-null aggregate
        // ClickHouse omits) decodes to a null cell rather than erroring.
        let columns = vec![column(AnalyticsFieldKind::Integer)];
        let rows = decode_json_each_row(b"{}\n", &columns).expect("decode rows");
        assert_eq!(rows, vec![vec![AnalyticsCell::Null]]);
    }

    #[test]
    fn empty_body_decodes_to_zero_rows_offline() {
        // Pins: an empty response body (no matching rows) yields no rows.
        let columns = vec![column(AnalyticsFieldKind::Integer)];
        let rows = decode_json_each_row(b"", &columns).expect("decode rows");
        assert!(rows.is_empty());
    }
}

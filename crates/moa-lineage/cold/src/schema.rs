//! Arrow schema for cold-tier lineage Parquet files.

use std::sync::Arc;

use arrow::datatypes::{DataType, Field, Schema, TimeUnit};

/// Returns the Arrow schema used for exported lineage rows.
#[must_use]
pub fn lineage_arrow_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("turn_id", DataType::FixedSizeBinary(16), false),
        Field::new("session_id", DataType::FixedSizeBinary(16), false),
        Field::new("user_id", DataType::Utf8, false),
        Field::new("workspace_id", DataType::Utf8, false),
        Field::new(
            "ts",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            false,
        ),
        Field::new("tier", DataType::Int16, false),
        Field::new("record_kind", DataType::Int16, false),
        Field::new("payload", DataType::Utf8, false),
        Field::new("answer_text", DataType::Utf8, true),
        Field::new("integrity_hash", DataType::FixedSizeBinary(32), false),
        Field::new("prev_hash", DataType::FixedSizeBinary(32), true),
    ]))
}

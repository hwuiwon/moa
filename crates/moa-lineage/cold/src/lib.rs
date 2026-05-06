//! S3 Parquet cold-tier export for lineage rows.
//!
//! The hot TimescaleDB store remains the write path. This crate owns the slower
//! worker that snapshots aged hot rows into Hive-partitioned object storage.

mod error;
mod exporter;
mod schema;

pub use error::{Error, Result};
pub use exporter::{ColdTierConfig, ColdTierExporter, ExporterStats, partition_key};
pub use schema::lineage_arrow_schema;

//! mpsc to fjall to TimescaleDB/ClickHouse lineage writer.

pub mod admin;

mod clickhouse;
mod error;
mod fjall_journal;
mod mpsc_sink;
pub mod otel;
mod schema;
mod store;
mod writer;

pub use clickhouse::{ClickHouseStore, LineageQueryFilters, LineageQueryRecord};
pub use error::{Error, Result};
pub use mpsc_sink::{MpscSink, MpscSinkConfig, NullSink, OtelSink};
pub use schema::ensure_schema;
pub use store::LineageStore;
pub use writer::{WriterHandle, WriterStats, spawn_writer};

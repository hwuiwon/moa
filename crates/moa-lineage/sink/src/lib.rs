//! Postgres-accepted lineage capture: bounded ingress, a durable acceptance
//! queue, and a leased drain into Postgres.

pub mod admin;

mod clickhouse;
mod error;
mod mpsc_sink;
pub mod otel;
mod store;
mod writer;

pub use clickhouse::{ClickHouseStore, LineageQueryFilters, LineageQueryRecord};
pub use error::{Error, Result};
pub use mpsc_sink::{MpscSink, MpscSinkConfig, NullSink, OtelSink};
pub use store::LineageStore;
pub use writer::{WriterHandle, WriterHealth, WriterState, WriterStats, spawn_writer};

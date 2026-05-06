//! mpsc to fjall to TimescaleDB lineage writer.

mod error;
mod fjall_journal;
mod mpsc_sink;
mod schema;
mod writer;

pub use error::{Error, Result};
pub use mpsc_sink::{MpscSink, MpscSinkBuilder, MpscSinkConfig, NullSink};
pub use schema::{SCHEMA_DDL, ensure_schema};
pub use writer::{LineageWriter, WriterHandle, WriterStats, spawn_writer};

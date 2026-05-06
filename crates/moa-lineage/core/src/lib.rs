//! Core lineage data model and sink trait.
//!
//! This crate is the type-stable foundation for `moa-lineage`. Other lineage
//! subcrates depend on it; it depends only on `moa-core` for shared identity
//! and scope types.

pub mod ids;
pub mod records;
pub mod sink;

pub use ids::{LineageRecordId, TurnId};
pub use records::{
    AgeIntrospection, BackendIntrospection, ContextChunk, ContextLineage, FusedHit,
    GenerationLineage, GraphPath, LineageEvent, PgvectorIntrospection, RecordKind, RerankHit,
    RetrievalLineage, RetrievalStage, StageTimings, TokenUsage, ToolCallSummary, TruncationEvent,
    TurbopufferIntrospection, VecHit,
};
pub use sink::{LineageSink, NullSink};

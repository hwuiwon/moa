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
    AclFilterDecision, AgeIntrospection, BackendIntrospection, Citation, CitationLineage,
    ContextChunk, ContextLineage, DecisionKind, DecisionRecord, FusedHit, GenerationLineage,
    GraphPath, LineageEvent, PgvectorIntrospection, PiiRedactionDecision, PrivacyEraseDecision,
    PrivacyExportDecision, RecordKind, RerankHit, RetrievalLineage, RetrievalStage,
    ScopeEnforcementDecision, ScoreRecord, ScoreSource, ScoreTarget, ScoreValue, StageTimings,
    TokenUsage, ToolCallSummary, TruncationEvent, TurbopufferIntrospection, VecHit, VerifierResult,
};
pub use sink::{LineageSink, NullSink};

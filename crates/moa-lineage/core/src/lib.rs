//! Core lineage data model and sink trait.
//!
//! This crate is the type-stable foundation for `moa-lineage`. Other lineage
//! subcrates depend on it; it depends only on `moa-core` for shared identity
//! and scope types.

pub mod chain;
pub mod ids;
pub mod records;
pub mod sink;

pub use ids::TurnId;
pub use records::{
    AclFilterDecision, BackendIntrospection, Citation, CitationLineage, ContextChunk,
    ContextLineage, DecisionKind, DecisionRecord, ExperimentScoreProvenance, ExperimentScoreTarget,
    FusedHit, GenerationLineage, GenerationTokenUsage, GraphIntrospection, GraphPath, LineageEvent,
    PgvectorIntrospection, PiiRedactionDecision, PrivacyEraseDecision, PrivacyExportDecision,
    RecordKind, RerankHit, RetrievalLineage, RetrievalSelectedHit, RetrievalStage,
    ScopeEnforcementDecision, ScoreRecord, ScoreSource, ScoreTarget, ScoreValue, StageTimings,
    ToolCallSummary, TruncationEvent, TurbopufferIntrospection, VecHit, VerifierResult,
};
pub use sink::LineageSink;

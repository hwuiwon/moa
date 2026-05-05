//! Graph-memory ingestion pipelines, fast-path APIs, and contradiction detection.

pub mod chunking;
pub mod connector;
pub mod contradiction;
pub mod ctx;
pub mod error;
pub mod extract;
pub mod fast_path;
pub mod slow_path;

pub use contradiction::{
    Conflict, ContradictionContext, ContradictionDetector, RrfPlusJudgeDetector,
};
pub use ctx::{
    IngestCtx, IngestRuntime, current_runtime, install_runtime, install_runtime_with_pool,
};
pub use error::{IngestError, Result};
pub use extract::{
    ClassifiedFact, EmbeddedFact, ExtractedFact, IngestApplyReport, IngestDecision, SessionTurn,
    TurnChunk, chunk_turn, extract_facts, fact_hash, fact_uid_from_hash, scoped_fact_uid,
    should_ingest_degraded,
};
pub use fast_path::{
    FastError, FastPathCtx, FastRememberRequest, ForgetPattern, execute_memory_tool, fast_forget,
    fast_remember, fast_supersede, is_fast_memory_tool,
};
pub use slow_path::{
    IngestionVO, IngestionVOClient, IngestionVOImpl, ingest_turn_direct, ingestion_object_key,
    turn_transcript,
};

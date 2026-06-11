//! Graph-memory ingestion pipelines, fast-path APIs, and contradiction detection.

pub mod chunking;
pub mod contradiction;
pub mod ctx;
pub mod entity_resolution;
pub mod error;
pub mod extract;
pub mod extractor;
pub mod fast_path;
pub mod llm_client;
pub mod llm_extractor;
pub mod recorded;
pub mod slow_path;

pub use contradiction::{
    Conflict, ContradictionContext, ContradictionDetector, RrfPlusJudgeDetector,
};
pub use ctx::{
    IngestCtx, IngestRuntime, current_runtime, install_runtime, install_runtime_with_config,
    install_runtime_with_pool,
};
pub use entity_resolution::{
    DeterministicEntityMergeVerifier, EntityMergeVerifier, EntityResolutionRequest, EntityResolver,
    ResolvedEntity,
};
pub use error::{IngestError, Result};
pub use extract::{
    ClassifiedFact, EmbeddedFact, ExtractedFact, ExtractedFactScopeHint, IngestApplyReport,
    IngestDecision, SessionTurn, TurnChunk, chunk_turn, extract_facts, extract_facts_checked,
    extraction_confidence_hint, fact_hash, fact_uid_from_hash, scoped_fact_uid,
    should_ingest_degraded,
};
pub use extractor::{FactExtractor, HeuristicFactExtractor, ScriptedFactExtractor};
pub use fast_path::{
    FastError, FastMemoryToolExecutor, FastPathCtx, FastRememberRequest, ForgetPattern,
    execute_memory_tool, fast_forget, fast_remember, fast_supersede, is_fast_memory_tool,
};
pub use llm_client::{LlmChatClient, LlmChatError};
pub use llm_extractor::{EXTRACTION_PROMPT_VERSION, LlmFactExtractor};
pub use recorded::{
    ExtractionFixtureRecord, RecordedExtractionStore, RecordedFact, RecordedFactExtractor,
    chunk_hash,
};
pub use slow_path::{
    IngestionVO, IngestionVOClient, IngestionVOImpl, ingest_turn_direct,
    ingest_turn_direct_with_ctx, ingest_turn_direct_with_pool, ingestion_object_key,
    turn_transcript,
};

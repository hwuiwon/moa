//! Context compilation, retrieval, and turn helpers for MOA.

pub mod compaction;
pub mod execution_planning;
#[cfg(feature = "eval-harness")]
pub mod harness;
pub mod learning;
pub mod lineage;
pub mod pipeline;
pub mod query_rewrite;
pub mod runtime_events;
pub mod segment_assessment;
mod text;
pub mod turn;
pub mod turn_learning;
pub mod turn_segments;

#[cfg(feature = "eval-harness")]
pub use harness::{
    BrainTurnRequest, StreamedTurnRequest, StreamedTurnResult, StreamedTurnSignalState, TurnResult,
    run_brain_turn, run_streamed_turn,
};
pub use pipeline::{
    ContextPipeline, DigestStageInput, GraphMemoryPipelineStages, GraphMemoryStageInput,
    HistoryStageInput, PipelineStageReport, QueryRewriteStageInput, RuntimeStageInput,
    SkillInjectionStageInput,
    build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions,
    build_default_graph_memory_retriever, build_graph_memory_retriever,
};
pub use turn::{StreamSignalDisposition, StreamedCompletion, stream_completion_response};

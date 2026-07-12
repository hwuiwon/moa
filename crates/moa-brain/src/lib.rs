//! Context compilation, retrieval, and turn helpers for MOA.

pub mod compaction;
#[cfg(feature = "eval-harness")]
pub mod harness;
pub mod learning;
pub mod lineage;
pub mod pipeline;
pub mod planning;
pub mod retrieval;
pub mod segment_assessment;
mod text;
pub mod turn;
pub mod turn_learning;
pub mod turn_segments;

#[cfg(feature = "eval-harness")]
pub use harness::{
    StreamedTurnResult, TurnResult, run_brain_turn, run_brain_turn_with_lineage, run_streamed_turn,
    run_streamed_turn_with_lineage, run_streamed_turn_with_signals,
    run_streamed_turn_with_signals_and_lineage,
};
pub use pipeline::{
    ContextPipeline, GraphMemoryPipelineOptions, PipelineStageReport,
    build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions,
    build_default_graph_memory_retriever, build_graph_memory_retriever,
};
pub use turn::{StreamSignalDisposition, StreamedCompletion, stream_completion_response};

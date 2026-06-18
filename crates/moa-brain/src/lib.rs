//! Context compilation, retrieval, and turn helpers for MOA.

pub mod compaction;
#[cfg(feature = "eval-harness")]
pub mod harness;
pub mod learning;
pub mod loop_detector;
pub mod pipeline;
pub mod planning;
pub mod retrieval;
pub mod segment_assessment;
pub mod tool_stats;
pub mod turn;

#[cfg(feature = "eval-harness")]
pub use harness::{
    StreamedTurnResult, TurnResult, run_brain_turn, run_streamed_turn,
    run_streamed_turn_with_signals,
};
pub use loop_detector::LoopDetector;
pub use pipeline::{
    ContextPipeline, GraphMemoryPipelineOptions, PipelineStageReport,
    build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions,
    build_default_graph_memory_retriever, build_default_pipeline,
    build_default_pipeline_with_tools,
};
pub use tool_stats::{ToolStats, WorkspaceToolStats, update_ema};
pub use turn::{StreamSignalDisposition, StreamedCompletion, stream_completion_response};

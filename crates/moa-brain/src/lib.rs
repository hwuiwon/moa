//! Brain harness and context compilation pipeline for MOA.

pub mod compaction;
pub mod harness;
pub mod intents;
pub mod loop_detector;
pub mod pipeline;
pub mod planning;
pub mod resolution;
pub mod retrieval;
pub mod tool_stats;
pub mod turn;

pub use compaction::maybe_compact;
pub use harness::{
    StreamedTurnResult, TurnResult, run_brain_turn, run_brain_turn_with_tools,
    run_brain_turn_with_tools_stepwise, run_streamed_turn, run_streamed_turn_with_signals,
    run_streamed_turn_with_signals_stepwise, run_streamed_turn_with_signals_stepwise_and_lineage,
};
pub use loop_detector::LoopDetector;
pub use pipeline::{
    ContextPipeline, GraphMemoryPipelineOptions, PipelineStageReport,
    build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions,
    build_default_pipeline, build_default_pipeline_with_tools,
};
pub use tool_stats::{ToolStats, WorkspaceToolStats, update_ema};
pub use turn::{
    PendingToolApproval, StoredApprovalDecision, StreamSignalDisposition, StreamedCompletion,
    find_pending_approval_request, find_pending_tool_approval, find_resolved_pending_tool_approval,
    stream_completion_response,
};

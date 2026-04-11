//! Brain harness and context compilation pipeline for MOA.

pub mod compaction;
pub mod harness;
pub mod pipeline;
pub mod tool_stats;
pub mod turn;

pub use compaction::maybe_compact;
pub use harness::{
    StreamedTurnResult, TurnResult, run_brain_turn, run_brain_turn_with_tools,
    run_brain_turn_with_tools_stepwise, run_streamed_turn, run_streamed_turn_with_signals,
};
pub use pipeline::{
    ContextPipeline, PipelineStageReport, build_default_pipeline,
    build_default_pipeline_with_runtime, build_default_pipeline_with_tools,
};
pub use tool_stats::{
    ToolStats, WorkspaceToolStats, load_workspace_tool_stats, update_ema,
    update_workspace_tool_stats, write_workspace_tool_stats,
};
pub use turn::{
    PendingToolApproval, StoredApprovalDecision, StreamSignalDisposition, StreamedCompletion,
    find_pending_approval_request, find_pending_tool_approval, find_resolved_pending_tool_approval,
    stream_completion_response,
};

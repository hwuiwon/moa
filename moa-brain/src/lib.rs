//! Brain harness and context compilation pipeline for MOA.

pub mod compaction;
pub mod harness;
pub mod pipeline;
pub mod turn;

pub use compaction::maybe_compact;
pub use harness::{
    TurnResult, run_brain_turn, run_brain_turn_with_tools, run_brain_turn_with_tools_stepwise,
};
pub use pipeline::{
    ContextPipeline, PipelineStageReport, build_default_pipeline, build_default_pipeline_with_tools,
};
pub use turn::{
    PendingToolApproval, StoredApprovalDecision, StreamSignalDisposition, StreamedCompletion,
    find_pending_approval_request, find_pending_tool_approval, find_resolved_pending_tool_approval,
    stream_completion_response,
};

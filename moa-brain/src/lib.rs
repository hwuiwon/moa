//! Brain harness and context compilation pipeline for MOA.

pub mod compaction;
pub mod harness;
pub mod pipeline;

pub use compaction::maybe_compact;
pub use harness::{TurnResult, run_brain_turn, run_brain_turn_with_tools};
pub use pipeline::{
    ContextPipeline, PipelineStageReport, build_default_pipeline, build_default_pipeline_with_tools,
};

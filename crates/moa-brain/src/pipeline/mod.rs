//! Context pipeline runner, processors, and stage assembly.

mod builder;
mod runner;

pub mod agent_instructions;
pub mod compactor;
pub mod digest;
pub mod history;
pub mod identity;
pub mod instructions;
pub mod memory;
pub mod query_rewrite;
pub mod runtime_context;
pub mod segments;
pub mod skills;
pub mod tools;

pub use builder::{
    GraphMemoryPipelineOptions,
    build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions,
    build_default_graph_memory_retriever, build_default_pipeline,
    build_default_pipeline_with_tools, build_graph_memory_retriever,
};
pub use runner::{ContextPipeline, PipelineStageReport};

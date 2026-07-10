//! Context pipeline runner, processors, and stage assembly.

mod builder;
mod runner;

pub mod agent_instructions;
pub mod delegation_planning;
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
    build_default_graph_memory_retriever, build_graph_memory_retriever,
};
pub use memory::{MemoryEvidenceRequest, MemoryEvidenceResponse, MemoryEvidenceSourceMetadata};
pub use runner::{ContextPipeline, PipelineStageReport};

/// Insertion point just before the trailing run of user messages.
///
/// Dynamic per-turn sections (skill manifest, memory digest, retrieved memory,
/// runtime reminder) insert here — after replayed history, immediately before
/// the active user turn — so their per-turn byte churn lands past the frozen
/// history region and provider prompt caches keep matching it.
pub(crate) fn trailing_user_insertion_index(messages: &[moa_core::ContextMessage]) -> usize {
    let mut insertion_index = messages.len();
    while insertion_index > 0
        && matches!(
            messages[insertion_index - 1].role,
            moa_core::MessageRole::User
        )
    {
        insertion_index -= 1;
    }
    insertion_index
}

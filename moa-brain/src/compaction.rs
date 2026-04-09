//! Context compaction stub used until full checkpointing lands.

use moa_core::{LLMProvider, Result, SessionId, SessionStore};

use crate::pipeline::ContextPipeline;

/// Placeholder compaction hook for future checkpoint-based context shrinking.
pub async fn maybe_compact(
    _store: &dyn SessionStore,
    _llm: &dyn LLMProvider,
    _session_id: SessionId,
    _pipeline: &ContextPipeline,
) -> Result<bool> {
    Ok(false)
}

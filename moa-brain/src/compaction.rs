//! Context compaction stub used until full checkpointing lands.

use moa_core::{LLMProvider, Result, SessionId, SessionStore};
use tracing::Instrument;

use crate::pipeline::ContextPipeline;

/// Placeholder compaction hook for future checkpoint-based context shrinking.
pub async fn maybe_compact(
    _store: &dyn SessionStore,
    _llm: &dyn LLMProvider,
    session_id: SessionId,
    _pipeline: &ContextPipeline,
) -> Result<bool> {
    let span = tracing::info_span!("compaction", moa.session.id = %session_id);
    async { Ok(false) }.instrument(span).await
}

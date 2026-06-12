//! Query-rewriting metadata shared across context pipeline stages.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Result produced by the query-rewrite context processor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryRewriteResult {
    /// Retrieval query used by graph-memory search.
    pub retrieval_query: String,
    /// Whether the original query was used or the LLM produced a rewritten query.
    pub source: RewriteSource,
    /// Reason the LLM rewrite was allowed to run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<RewriteReason>,
    /// Whether this message starts a new task segment.
    #[serde(default)]
    pub is_new_task: bool,
    /// Short summary of the new task when a segment transition is detected.
    #[serde(default)]
    pub task_summary: Option<String>,
}

impl QueryRewriteResult {
    /// Creates a fail-open result that preserves the original query.
    #[must_use]
    pub fn original(query: impl Into<String>) -> Self {
        Self {
            retrieval_query: query.into(),
            source: RewriteSource::Original,
            reason: None,
            is_new_task: false,
            task_summary: None,
        }
    }

    /// Creates a rewritten result with the supplied gate reason.
    #[must_use]
    pub fn rewritten(query: impl Into<String>, reason: RewriteReason) -> Self {
        Self {
            retrieval_query: query.into(),
            source: RewriteSource::Rewritten,
            reason: Some(reason),
            is_new_task: false,
            task_summary: None,
        }
    }

    /// Returns the provider-facing JSON Schema for rewriter model output.
    #[must_use]
    pub fn response_schema() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "retrieval_query": { "type": "string" },
                "is_new_task": { "type": "boolean" },
                "task_summary": {
                    "type": ["string", "null"]
                }
            },
            "required": [
                "retrieval_query",
                "is_new_task",
                "task_summary"
            ]
        })
    }
}

/// Reason the query rewrite gate allowed an LLM rewrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewriteReason {
    /// The current query contains deictic or coreference terms and history can resolve them.
    CoreferenceWithHistory,
    /// The current query is a short follow-up with history and no standalone anchors.
    VagueFollowup,
    /// The query is history, similarity, or preference shaped and benefits from vector retrieval.
    VectorFirstSemantic,
    /// The query asks for relation traversal or multi-hop synthesis without clear seed anchors.
    MultiHopWithoutSeeds,
}

/// Source of the query-rewrite metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewriteSource {
    /// The original query is used for retrieval.
    Original,
    /// The query was rewritten by the rewriter model.
    Rewritten,
}

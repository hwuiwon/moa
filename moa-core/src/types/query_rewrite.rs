//! Query-rewriting metadata shared across context pipeline stages.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Result produced by the query-rewrite context processor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryRewriteResult {
    /// The self-contained rewritten query. Never adds new entities.
    pub rewritten_query: String,
    /// Extracted intent classification.
    pub intent: QueryIntent,
    /// Optional sub-queries for compound tasks.
    pub sub_queries: Vec<String>,
    /// Tool names the rewriter thinks are relevant.
    pub suggested_tools: Vec<String>,
    /// Whether the rewriter thinks clarification is needed.
    pub needs_clarification: bool,
    /// If clarification is needed, the question to ask.
    #[serde(default)]
    pub clarification_question: Option<String>,
    /// Whether the rewriter ran or fell back to the original query.
    pub source: RewriteSource,
}

impl QueryRewriteResult {
    /// Creates a fail-open passthrough result that preserves the original query.
    #[must_use]
    pub fn passthrough(query: impl Into<String>) -> Self {
        Self {
            rewritten_query: query.into(),
            intent: QueryIntent::Unknown,
            sub_queries: Vec::new(),
            suggested_tools: Vec::new(),
            needs_clarification: false,
            clarification_question: None,
            source: RewriteSource::Passthrough,
        }
    }

    /// Returns the provider-facing JSON Schema for rewriter model output.
    #[must_use]
    pub fn response_schema() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "rewritten_query": { "type": "string" },
                "intent": {
                    "type": "string",
                    "enum": [
                        "coding",
                        "research",
                        "file_operation",
                        "system_admin",
                        "creative",
                        "question",
                        "conversation",
                        "unknown"
                    ]
                },
                "sub_queries": {
                    "type": "array",
                    "items": { "type": "string" }
                },
                "suggested_tools": {
                    "type": "array",
                    "items": { "type": "string" }
                },
                "needs_clarification": { "type": "boolean" },
                "clarification_question": {
                    "type": ["string", "null"]
                }
            },
            "required": [
                "rewritten_query",
                "intent",
                "sub_queries",
                "suggested_tools",
                "needs_clarification",
                "clarification_question"
            ]
        })
    }
}

/// High-level user intent classification inferred by query rewriting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryIntent {
    /// Coding or software-engineering work.
    Coding,
    /// Research, lookup, or synthesis work.
    Research,
    /// File creation, reading, editing, or movement.
    FileOperation,
    /// System administration or deployment work.
    SystemAdmin,
    /// Creative writing or generation.
    Creative,
    /// A direct question.
    Question,
    /// Conversational or social exchange.
    Conversation,
    /// Unknown or ambiguous intent.
    Unknown,
}

/// Source of the query-rewrite metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewriteSource {
    /// The query was rewritten by the rewriter model.
    Rewritten,
    /// The original query was passed through after a skip or failure.
    Passthrough,
}

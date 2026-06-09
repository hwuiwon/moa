//! Query-rewriting metadata shared across context pipeline stages.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Result produced by the query-rewrite context processor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryRewriteResult {
    /// The self-contained rewritten query. Never adds new entities.
    pub rewritten_query: String,
    /// Coarse advisory task kind for observability and prompt preparation.
    pub task_kind: TaskKind,
    /// Optional sub-queries for compound tasks.
    pub sub_queries: Vec<String>,
    /// Advisory tool hints retained for compatibility; the main agent still chooses actions.
    pub suggested_tools: Vec<String>,
    /// Whether the task likely needs fresh external information before answering.
    #[serde(default)]
    pub freshness_required: bool,
    /// Whether the task likely needs repository or workspace inspection before answering.
    #[serde(default)]
    pub repo_context_required: bool,
    /// Advisory memory action inferred from the user request.
    #[serde(default)]
    pub memory_action: MemoryAction,
    /// Whether the rewriter thinks clarification is needed.
    pub needs_clarification: bool,
    /// If clarification is needed, the question to ask.
    #[serde(default)]
    pub clarification_question: Option<String>,
    /// Whether this message starts a new task segment.
    #[serde(default)]
    pub is_new_task: bool,
    /// Short summary of the new task when a segment transition is detected.
    #[serde(default)]
    pub task_summary: Option<String>,
    /// Advisory tool-selection biases for prompt preparation.
    #[serde(default)]
    pub tool_bias: Vec<String>,
    /// Advisory promptlets that downstream preparation may prefer for this task.
    #[serde(default)]
    pub suggested_promptlets: Vec<String>,
    /// Whether the rewriter ran or fell back to the original query.
    pub source: RewriteSource,
}

impl QueryRewriteResult {
    /// Creates a fail-open passthrough result that preserves the original query.
    #[must_use]
    pub fn passthrough(query: impl Into<String>) -> Self {
        Self {
            rewritten_query: query.into(),
            task_kind: TaskKind::Unknown,
            sub_queries: Vec::new(),
            suggested_tools: Vec::new(),
            freshness_required: false,
            repo_context_required: false,
            memory_action: MemoryAction::None,
            needs_clarification: false,
            clarification_question: None,
            is_new_task: false,
            task_summary: None,
            tool_bias: Vec::new(),
            suggested_promptlets: Vec::new(),
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
                "task_kind": {
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
                "freshness_required": { "type": "boolean" },
                "repo_context_required": { "type": "boolean" },
                "memory_action": {
                    "type": "string",
                    "enum": [
                        "none",
                        "retrieve",
                        "remember",
                        "forget",
                        "supersede",
                        "ingest"
                    ]
                },
                "needs_clarification": { "type": "boolean" },
                "clarification_question": {
                    "type": ["string", "null"]
                },
                "is_new_task": { "type": "boolean" },
                "task_summary": {
                    "type": ["string", "null"]
                },
                "tool_bias": {
                    "type": "array",
                    "items": { "type": "string" }
                },
                "suggested_promptlets": {
                    "type": "array",
                    "items": { "type": "string" }
                }
            },
            "required": [
                "rewritten_query",
                "task_kind",
                "sub_queries",
                "suggested_tools",
                "freshness_required",
                "repo_context_required",
                "memory_action",
                "needs_clarification",
                "clarification_question",
                "is_new_task",
                "task_summary",
                "tool_bias",
                "suggested_promptlets"
            ]
        })
    }
}

/// Advisory memory action inferred by query rewriting.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryAction {
    /// No memory-specific action is requested.
    #[default]
    None,
    /// Retrieve existing memory before answering.
    Retrieve,
    /// Store a short fact, preference, or lesson.
    Remember,
    /// Remove remembered information.
    Forget,
    /// Replace or update remembered information.
    Supersede,
    /// Ingest longer reference material into memory.
    Ingest,
}

/// Coarse task category inferred by query rewriting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
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
    /// Unknown or ambiguous task kind.
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

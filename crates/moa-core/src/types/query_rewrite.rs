//! Query-rewriting metadata shared across context pipeline stages.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::experience::TaskFacetSet;

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
    /// Optional deterministic task facets supplied by the rewriter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_facets: Option<TaskFacetSet>,
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
            task_facets: None,
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
            task_facets: None,
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
                },
                "task_facets": {
                    "type": ["object", "null"],
                    "additionalProperties": false,
                    "properties": {
                        "domain": { "type": ["string", "null"] },
                        "action": { "type": ["string", "null"] },
                        "artifact_kind": { "type": ["string", "null"] },
                        "language_or_framework": { "type": ["string", "null"] },
                        "verification_style": { "type": ["string", "null"] },
                        "risk_class": { "type": ["string", "null"] },
                        "tool_pattern": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "skill_pattern": {
                            "type": "array",
                            "items": { "type": "string" }
                        }
                    },
                    "required": [
                        "domain",
                        "action",
                        "artifact_kind",
                        "language_or_framework",
                        "verification_style",
                        "risk_class",
                        "tool_pattern",
                        "skill_pattern"
                    ]
                }
            },
            "required": [
                "retrieval_query",
                "is_new_task",
                "task_summary",
                "task_facets"
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

#[cfg(test)]
mod tests {
    use super::QueryRewriteResult;

    #[test]
    fn response_schema_requires_nullable_fields_for_strict_providers() {
        // Pins: strict structured-output providers require every declared object property.
        let schema = QueryRewriteResult::response_schema();
        assert_eq!(
            schema["required"],
            serde_json::json!([
                "retrieval_query",
                "is_new_task",
                "task_summary",
                "task_facets"
            ])
        );
        assert_eq!(
            schema["properties"]["task_facets"]["required"],
            serde_json::json!([
                "domain",
                "action",
                "artifact_kind",
                "language_or_framework",
                "verification_style",
                "risk_class",
                "tool_pattern",
                "skill_pattern"
            ])
        );
        assert!(
            schema["properties"].get("complexity_hint").is_none(),
            "query rewrite remains retrieval-scoped, not an intent router"
        );
    }
}

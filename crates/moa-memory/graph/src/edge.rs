//! Edge labels and write intents for graph-memory relationships.

use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{GraphError, Result};

/// Supported Apache AGE edge labels for graph memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "SCREAMING_SNAKE_CASE")]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EdgeLabel {
    /// Generic semantic relationship.
    RelatesTo,
    /// Dependency relationship.
    DependsOn,
    /// Supersession relationship.
    Supersedes,
    /// Contradiction relationship.
    Contradicts,
    /// Derivation relationship.
    DerivedFrom,
    /// Source mention relationship.
    MentionedIn,
    /// Causal relationship.
    Caused,
    /// Lesson provenance relationship.
    LearnedFrom,
    /// Applicability relationship.
    AppliesTo,
}

impl EdgeLabel {
    /// Returns the canonical AGE label string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RelatesTo => "RELATES_TO",
            Self::DependsOn => "DEPENDS_ON",
            Self::Supersedes => "SUPERSEDES",
            Self::Contradicts => "CONTRADICTS",
            Self::DerivedFrom => "DERIVED_FROM",
            Self::MentionedIn => "MENTIONED_IN",
            Self::Caused => "CAUSED",
            Self::LearnedFrom => "LEARNED_FROM",
            Self::AppliesTo => "APPLIES_TO",
        }
    }
}

impl FromStr for EdgeLabel {
    type Err = GraphError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "RELATES_TO" => Ok(Self::RelatesTo),
            "DEPENDS_ON" => Ok(Self::DependsOn),
            "SUPERSEDES" => Ok(Self::Supersedes),
            "CONTRADICTS" => Ok(Self::Contradicts),
            "DERIVED_FROM" => Ok(Self::DerivedFrom),
            "MENTIONED_IN" => Ok(Self::MentionedIn),
            "CAUSED" => Ok(Self::Caused),
            "LEARNED_FROM" => Ok(Self::LearnedFrom),
            "APPLIES_TO" => Ok(Self::AppliesTo),
            other => Err(GraphError::UnknownEdgeLabel(other.to_string())),
        }
    }
}

/// Intent to create one AGE relationship between two graph nodes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdgeWriteIntent {
    /// Stable external edge identity.
    pub uid: Uuid,
    /// AGE edge label.
    pub label: EdgeLabel,
    /// Start node uid.
    pub start_uid: Uuid,
    /// End node uid.
    pub end_uid: Uuid,
    /// Relationship properties serialized into AGE `agtype`.
    pub properties: serde_json::Value,
    /// Workspace scope for workspace and user rows.
    pub workspace_id: Option<String>,
    /// User scope inside a workspace for user-private rows.
    pub user_id: Option<String>,
    /// Expected scope tier: `global`, `workspace`, or `user`.
    pub scope: String,
    /// Principal identifier that triggered the mutation.
    pub actor_id: String,
    /// Principal kind written to the graph changelog.
    pub actor_kind: String,
}

//! Edge labels and write intents for graph-memory relationships.

use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{GraphError, Result};

/// Supported edge labels for graph memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "SCREAMING_SNAKE_CASE")]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EdgeLabel {
    /// Generic semantic relationship.
    RelatesTo,
    /// Dependency relationship.
    DependsOn,
    /// Ownership or stewardship relationship.
    OwnedBy,
    /// Supersession relationship.
    Supersedes,
    /// Contradiction relationship.
    Contradicts,
    /// Derivation relationship.
    DerivedFrom,
    /// Containment relationship for tenant knowledge source, document, and chunk chains.
    Contains,
    /// Source mention relationship.
    MentionedIn,
    /// Contact-group membership relationship.
    MemberOf,
    /// Causal relationship.
    Caused,
    /// Lesson provenance relationship.
    LearnedFrom,
    /// Applicability relationship.
    AppliesTo,
}

impl EdgeLabel {
    /// Every supported graph edge label.
    pub const ALL: [Self; 12] = [
        Self::RelatesTo,
        Self::DependsOn,
        Self::OwnedBy,
        Self::Supersedes,
        Self::Contradicts,
        Self::DerivedFrom,
        Self::Contains,
        Self::MentionedIn,
        Self::MemberOf,
        Self::Caused,
        Self::LearnedFrom,
        Self::AppliesTo,
    ];

    /// Returns the canonical SQL label string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RelatesTo => "RELATES_TO",
            Self::DependsOn => "DEPENDS_ON",
            Self::OwnedBy => "OWNED_BY",
            Self::Supersedes => "SUPERSEDES",
            Self::Contradicts => "CONTRADICTS",
            Self::DerivedFrom => "DERIVED_FROM",
            Self::Contains => "CONTAINS",
            Self::MentionedIn => "MENTIONED_IN",
            Self::MemberOf => "MEMBER_OF",
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
            "OWNED_BY" => Ok(Self::OwnedBy),
            "SUPERSEDES" => Ok(Self::Supersedes),
            "CONTRADICTS" => Ok(Self::Contradicts),
            "DERIVED_FROM" => Ok(Self::DerivedFrom),
            "CONTAINS" => Ok(Self::Contains),
            "MENTIONED_IN" => Ok(Self::MentionedIn),
            "MEMBER_OF" => Ok(Self::MemberOf),
            "CAUSED" => Ok(Self::Caused),
            "LEARNED_FROM" => Ok(Self::LearnedFrom),
            "APPLIES_TO" => Ok(Self::AppliesTo),
            other => Err(GraphError::UnknownEdgeLabel(other.to_string())),
        }
    }
}

/// Intent to create one relationship between two graph nodes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdgeWriteIntent {
    /// Stable external edge identity.
    pub uid: Uuid,
    /// Graph edge label.
    pub label: EdgeLabel,
    /// Start node uid.
    pub start_uid: Uuid,
    /// End node uid.
    pub end_uid: Uuid,
    /// Relationship properties stored in the relational edge row.
    pub properties: serde_json::Value,
    /// Storage partition scope for tenant and contact rows.
    pub storage_partition_id: Option<String>,
    /// Contact scope inside a tenant for contact-private rows.
    pub contact_id: Option<String>,
    /// Expected scope tier: `global`, `tenant`, or `contact`.
    pub scope: String,
    /// Principal identifier that triggered the mutation.
    pub actor_id: String,
    /// Principal kind written to the graph changelog.
    pub actor_kind: String,
}
